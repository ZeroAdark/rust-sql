//! Hand adapters over the jsonpath bodies: the wrapper builds a private `PathArgs` the translator
//! cannot name; the shim opens the three images itself and calls the same `*_core` routines.

use adt_jsonpath_exec::JsonPathVars;

use crate::{fail, null_ret, ok, region_copy, with_scratch, ErrSlot, Region, Ret, View};

macro_rules! path_bool {
    ($($name:ident: $core:ident, $vars:expr, $tz:literal;)*) => {$(
        pub extern "C-unwind" fn $name(e: *mut ErrSlot, a0: View, a1: View, a2: View, a3: i64) -> Ret {
            with_scratch(|mcx| {
                let jb = match varlena::open_image(mcx, a0.bytes()) { Ok(v) => v, Err(err) => return fail(e, err) };
                let vars = match varlena::open_image(mcx, a2.bytes()) { Ok(v) => v, Err(err) => return fail(e, err) };
                let vars = if $vars { JsonPathVars::Jsonb(vars.as_bytes()) } else { JsonPathVars::None };
                let silent = if $vars { a3 != 0 } else { true };
                match adt_jsonpath_exec::$core(mcx, jb.as_bytes(), a1.bytes(), vars, silent, $tz) {
                    Ok(Some(b)) => ok(b as i64),
                    Ok(None) => null_ret(),
                    Err(err) => fail(e, err),
                }
            })
        }
    )*};
}

path_bool! {
    jsonb_path_exists: jsonb_path_exists_core, true, false;
    jsonb_path_exists_tz: jsonb_path_exists_core, true, true;
    jsonb_path_match: jsonb_path_match_core, true, false;
    jsonb_path_match_tz: jsonb_path_match_core, true, true;
}

macro_rules! path_bool_opr {
    ($($name:ident: $core:ident;)*) => {$(
        pub extern "C-unwind" fn $name(e: *mut ErrSlot, a0: View, a1: View) -> Ret {
            with_scratch(|mcx| {
                let jb = match varlena::open_image(mcx, a0.bytes()) { Ok(v) => v, Err(err) => return fail(e, err) };
                match adt_jsonpath_exec::$core(mcx, jb.as_bytes(), a1.bytes(), JsonPathVars::None, true, false) {
                    Ok(Some(b)) => ok(b as i64),
                    Ok(None) => null_ret(),
                    Err(err) => fail(e, err),
                }
            })
        }
    )*};
}

path_bool_opr! {
    jsonb_path_exists_opr: jsonb_path_exists_core;
    jsonb_path_match_opr: jsonb_path_match_core;
}

macro_rules! path_image {
    ($($name:ident: $core:ident, $tz:literal, $wrap:expr;)*) => {$(
        pub extern "C-unwind" fn $name(e: *mut ErrSlot, r: *mut Region, a0: View, a1: View, a2: View, a3: i64) -> Ret {
            with_scratch(|mcx| {
                let jb = match varlena::open_image(mcx, a0.bytes()) { Ok(v) => v, Err(err) => return fail(e, err) };
                let vars = match varlena::open_image(mcx, a2.bytes()) { Ok(v) => v, Err(err) => return fail(e, err) };
                match adt_jsonpath_exec::$core(mcx, jb.as_bytes(), a1.bytes(), JsonPathVars::Jsonb(vars.as_bytes()), a3 != 0, $tz).map($wrap) {
                    Ok(Some(img)) => region_copy(r, &img),
                    Ok(None) => null_ret(),
                    Err(err) => fail(e, err),
                }
            })
        }
    )*};
}

path_image! {
    jsonb_path_query_array: jsonb_path_query_array_core, false, Some;
    jsonb_path_query_array_tz: jsonb_path_query_array_core, true, Some;
    jsonb_path_query_first: jsonb_path_query_first_core, false, |v| v;
    jsonb_path_query_first_tz: jsonb_path_query_first_core, true, |v| v;
}
