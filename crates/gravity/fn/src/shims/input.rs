//! Hand adapters over the one-argument type input functions a `CoerceViaIO` cast calls per row (`::uuid`, `::int4`,
//! `::bigint`, ...). The fmgr wrappers read the soft-error context, so the generator keeps them on the function manager;
//! a cast calls them with none (C `InputFunctionCall`), where the body raises its error as PostgreSQL does, so these
//! adapters call the same bodies with no context. Text is the server-encoding cstring, read as the wrappers read it: the raw
//! bytes where the wrapper hands its body the cstring's bytes, `text` where it hands it the lossy `str`.

use std::borrow::Cow;

use crate::{fail, ok, region_copy, ErrSlot, Region, Ret, View};

/// The text as the wrappers read it (`String::from_utf8_lossy`), checked by `str::from_utf8` first: its ASCII run is
/// word-at-a-time, where the lossy chunk iterator walks byte runs (a tenth of a hosted `::numeric` cast).
#[inline(always)]
pub(crate) fn text(a: View) -> Cow<'static, str> {
    match core::str::from_utf8(a.bytes()) {
        Ok(s) => Cow::Borrowed(s),
        Err(_) => String::from_utf8_lossy(a.bytes()),
    }
}

pub extern "C-unwind" fn int2in(e: *mut ErrSlot, a0: View) -> Ret {
    match adt_int::int2in(a0.bytes(), None) {
        Ok(v) => ok(v as i64),
        Err(err) => fail(e, err),
    }
}

pub extern "C-unwind" fn int4in(e: *mut ErrSlot, a0: View) -> Ret {
    match adt_int::int4in(a0.bytes(), None) {
        Ok(v) => ok(v as i64),
        Err(err) => fail(e, err),
    }
}

pub extern "C-unwind" fn int8in(e: *mut ErrSlot, a0: View) -> Ret {
    match adt_int8::int8in(a0.bytes(), None) {
        Ok(v) => ok(v),
        Err(err) => fail(e, err),
    }
}

pub extern "C-unwind" fn oidin(e: *mut ErrSlot, a0: View) -> Ret {
    match numutils::uint32in_subr_bytes(a0.bytes(), false, "oid", None) {
        Ok((v, _)) => ok(v as i64),
        Err(err) => fail(e, err),
    }
}

pub extern "C-unwind" fn boolin(e: *mut ErrSlot, a0: View) -> Ret {
    match adt_bool::boolin(a0.bytes(), None) {
        Ok(v) => ok(v as i64),
        Err(err) => fail(e, err),
    }
}

pub extern "C-unwind" fn float4in(e: *mut ErrSlot, a0: View) -> Ret {
    // the raw bytes ride along as orig_string (the error quotes them), as `fc_float4in` passes them
    match adt_float::float4in_internal(&text(a0), None, "real", a0.bytes(), None) {
        Ok(v) => ok(v.to_bits() as i64),
        Err(err) => fail(e, err),
    }
}

pub extern "C-unwind" fn float8in(e: *mut ErrSlot, a0: View) -> Ret {
    match adt_float::float8in_internal(&text(a0), None, "double precision", a0.bytes(), None) {
        Ok(v) => ok(v.to_bits() as i64),
        Err(err) => fail(e, err),
    }
}

pub extern "C-unwind" fn uuid_in(e: *mut ErrSlot, r: *mut Region, a0: View) -> Ret {
    match adt_uuid::uuid_in(a0.bytes(), None) {
        Ok(v) => region_copy(r, &v[..]),
        Err(err) => fail(e, err),
    }
}

// The date / time input functions: the same bodies the fmgr wrappers call (`fc_date_in`,
// `fc_timestamp_in`, ...), with no soft-error context, as a cast's InputFunctionCall passes none. Their session reads
// (DateStyle, the time zone) are the bodies' own, as on the function manager.

pub extern "C-unwind" fn date_in(e: *mut ErrSlot, a0: View) -> Ret {
    match adt_date::date_in(a0.bytes(), None) {
        Ok(v) => ok(v as i64),
        Err(err) => fail(e, err),
    }
}

pub extern "C-unwind" fn time_in(e: *mut ErrSlot, a0: View, _a1: i64, a2: i64) -> Ret {
    match adt_date::time_in(a0.bytes(), a2 as i32, None) {
        Ok(v) => ok(v),
        Err(err) => fail(e, err),
    }
}

pub extern "C-unwind" fn timestamp_in(e: *mut ErrSlot, a0: View, _a1: i64, a2: i64) -> Ret {
    match adt_timestamp::timestamp_in(a0.bytes(), a2 as i32, None) {
        Ok(v) => ok(v),
        Err(err) => fail(e, err),
    }
}

pub extern "C-unwind" fn timestamptz_in(e: *mut ErrSlot, a0: View, _a1: i64, a2: i64) -> Ret {
    match adt_timestamp::timestamptz_in(a0.bytes(), a2 as i32, None) {
        Ok(v) => ok(v),
        Err(err) => fail(e, err),
    }
}

pub extern "C-unwind" fn interval_in(e: *mut ErrSlot, r: *mut Region, a0: View, _a1: i64, a2: i64) -> Ret {
    match adt_timestamp::interval::interval_in(&text(a0), a2 as i32, None) {
        Ok(v) => crate::region_interval(r, &v),
        Err(err) => fail(e, err),
    }
}

// The byte / network / JSON input functions: the bodies their fmgr wrappers call (`fc_byteain`, `fc_inet_in`,
// `fc_jsonb_in`, ...), with no soft-error context, as a cast's InputFunctionCall passes none: a malformed text raises
// the body's own error. With no context a body never answers "absorbed" (`Ok(None)`); that answer is a named error.

#[cold]
#[inline(never)]
fn absorbed(f: &str) -> Box<crate::PgError> {
    Box::new(crate::PgError::error(format!("{f}: soft-error escape without an escontext"))) // alloc: Cold
}

pub extern "C-unwind" fn byteain(e: *mut ErrSlot, r: *mut Region, a0: View) -> Ret {
    crate::with_scratch(|mcx| match varlena::bytea::byteain(mcx, a0.bytes(), None) {
        Ok(Some(v)) => crate::region_varlena(r, v),
        Ok(None) => fail(e, absorbed("byteain")),
        Err(err) => fail(e, err),
    })
}

fn network_in(e: *mut ErrSlot, r: *mut Region, a0: View, is_cidr: bool) -> Ret {
    match adt_network::network_in(a0.bytes(), is_cidr, None) {
        Ok(Some(v)) => {
            let (img, len) = v.image();
            region_copy(r, &img[..len])
        }
        Ok(None) => fail(e, absorbed(if is_cidr { "cidr_in" } else { "inet_in" })),
        Err(err) => fail(e, err),
    }
}

pub extern "C-unwind" fn inet_in(e: *mut ErrSlot, r: *mut Region, a0: View) -> Ret {
    network_in(e, r, a0, false)
}

pub extern "C-unwind" fn cidr_in(e: *mut ErrSlot, r: *mut Region, a0: View) -> Ret {
    network_in(e, r, a0, true)
}

pub extern "C-unwind" fn macaddr_in(e: *mut ErrSlot, r: *mut Region, a0: View) -> Ret {
    match adt_mac::macaddr_in(a0.bytes(), None) {
        Ok(v) => region_copy(r, &v.to_bytes()),
        Err(err) => fail(e, err),
    }
}

pub extern "C-unwind" fn jsonb_in(e: *mut ErrSlot, r: *mut Region, a0: View) -> Ret {
    crate::with_scratch(|mcx| match adt_jsonb::io::jsonb_in(mcx, a0.bytes(), None) {
        Ok(Some(v)) => region_copy(r, &v),
        Ok(None) => fail(e, absorbed("jsonb_in")),
        Err(err) => fail(e, err),
    })
}

pub extern "C-unwind" fn json_in(e: *mut ErrSlot, r: *mut Region, a0: View) -> Ret {
    crate::with_scratch(|mcx| match adt_json::json_in(mcx, a0.bytes(), None) {
        Ok(Some(v)) => crate::region_varlena(r, v),
        Ok(None) => fail(e, absorbed("json_in")),
        Err(err) => fail(e, err),
    })
}
