//! Hand adapters over the jsonb field readers. `jsonb_object_field_text` (`->>` with a text key)
//! takes its jsonb as the stored Datum (`ArgKind::D`): a short- or plain-header value is read in place, and a string,
//! boolean or null answer goes straight into the caller's region (one copy, PostgreSQL's `cstring_to_text`), with no
//! scratch context and no image rebuilt in the region first. A compressed or external value, and a numeric or container
//! answer, take the general road: the value opened as the function manager's wrapper opens it (a short header rebuilt
//! as a 4-byte one, so a numeric's digits are aligned), the body's text result copied into the region.

use adt_jsonb::container::{container_is_object, get_key_value, JsonbItem};

use crate::{fail, null_ret, region_datum, region_payload, scratch_image, with_scratch, ErrSlot, Region, Ret, View};

pub extern "C-unwind" fn jsonb_object_field_text(e: *mut ErrSlot, r: *mut Region, _coll: u32, a0: i64, a1: View) -> Ret {
    let image = super::raw::varlena_image(a0);
    let b0 = image[0];
    // short (1-byte) or plain 4-byte header, neither compressed nor external: the container is the payload in place
    if !(b0 == 0x01 || (b0 & 0x03) == 0x02) {
        let c = if b0 & 0x01 == 0x01 {
            &image[1..]
        } else {
            &image[4..]
        };
        if !container_is_object(c) {
            return null_ret();
        }
        match get_key_value(c, a1.bytes()) {
            None | Some(JsonbItem::Null) => return null_ret(),
            Some(JsonbItem::String(s)) => return region_payload(r, s),
            Some(JsonbItem::Bool(true)) => return region_payload(r, b"true"),
            Some(JsonbItem::Bool(false)) => return region_payload(r, b"false"),
            Some(_) => {}
        }
    }
    with_scratch(|mcx| {
        // a short-header value is rebuilt with a 4-byte header first, as the wrapper's detoast gives it: a numeric
        // answer's digits are read as aligned words
        if b0 & 0x01 == 0x01 && b0 != 0x01 {
            let img = scratch_image(mcx, &image[1..]);
            return field_text(e, r, mcx, &img[4..], a1);
        }
        let jb = match varlena::open_image(mcx, image) {
            Ok(v) => v,
            Err(err) => return fail(e, err.into()),
        };
        field_text(e, r, mcx, jb.as_bytes(), a1)
    })
}

fn field_text(e: *mut ErrSlot, r: *mut Region, mcx: mcx::Mcx<'_>, payload: &[u8], a1: View) -> Ret {
    match adt_jsonb::getfield::object_field_text(mcx, payload, a1.bytes()) {
        Ok(Some(v)) => region_datum(r, fmgr::varlena_result(v).as_usize() as *const u8, -1),
        Ok(None) => null_ret(),
        Err(err) => fail(e, err.into()),
    }
}
