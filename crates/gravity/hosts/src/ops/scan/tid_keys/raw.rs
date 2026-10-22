//! tid_keys' unsafe (lint R6; construct bundle scan-05; its raw.rs child): `tid_array` and `cursor_of` moved as is from
//! tid_keys.rs with their safety contracts, and the TID sources' key-cell reads over them (tid_sample.rs's, moved as is
//! from its `eval_range` / `eval_points`), whose callers keep their reasons beside the call.
#![allow(unsafe_code)]
use gravity_abi::Datum as Word;
use types_error::{PgError, PgResult, ERRCODE_UNDEFINED_CURSOR};
use types_tuple::ItemPointerData;

/// A tid key cell's ItemPointerData image. The caller passes a non-null tid key cell: a pointer to an ItemPointerData
/// image.
pub(crate) fn tid_cell(k: Word) -> ItemPointerData {
    // SAFETY: the caller's contract above.
    unsafe { core::ptr::read_unaligned(k as *const ItemPointerData) }
}

/// `tid_array` of an array key cell. The caller passes a non-null array key cell: the address of a live array image.
pub(crate) fn tid_array_cell(k: Word, out: &mut Vec<ItemPointerData>) -> PgResult<()> {
    // SAFETY: the caller's contract above.
    unsafe { tid_array(k as *const u8, out) }
}

/// `cursor_of` a CURRENT OF clause's key cell. The caller passes the clause's non-null key cell: the bind's cursor word
/// (`TidClause::Current`), `by_param` as the clause says.
pub(crate) fn cursor_cell<'m>(mcx: mcx::Mcx<'m>, k: Word, by_param: bool) -> PgResult<&'m str> {
    // SAFETY: the caller's contract above.
    unsafe { cursor_of(mcx, k, by_param) }
}

/// A 1-D `tid[]` image's non-NULL elements (6 bytes each, 2-aligned; `ARR_*` layout); a compressed or external image refuses.
/// # Safety
/// `p` addresses a live array varlena (a non-null key cell).
pub(crate) unsafe fn tid_array(p: *const u8, out: &mut Vec<ItemPointerData>) -> PgResult<()> {
    if *p & 0x03 != 0 {
        return Err(crate::pg_err("gravity: a TID array whose image is compressed, short or external"));
    }
    let rd = |o: usize| i32::from_le_bytes([*p.add(o), *p.add(o + 1), *p.add(o + 2), *p.add(o + 3)]);
    match rd(4) {
        0 => return Ok(()),
        1 => {}
        _ => return Err(crate::pg_err("gravity: a TID array of more than one dimension")),
    }
    let (dataoffset, n) = (rd(8) as usize, rd(16) as usize);
    let nulls = (dataoffset != 0).then(|| p.add(24));
    let mut d = if dataoffset != 0 { dataoffset } else { 24 };
    // an element is the unit (a 6-byte copy, so a tick per batch): the array is as long as its value
    let mut step = cancelpoll::step("tid_keys tid_array: one element of a tid array");
    for i in 0..n {
        step.batch(i).map_err(|_| crate::pg_err("gravity: a TID array's read stopped for its statement's cancel"))?;
        if nulls.is_some_and(|b| *b.add(i / 8) & (1 << (i % 8)) == 0) {
            continue;
        }
        d = (d + 1) & !1usize;
        out.push(core::ptr::read_unaligned(p.add(d) as *const ItemPointerData)); // alloc: Exec
        d += 6;
    }
    Ok(())
}

/// D422: the cursor a `CURRENT OF` key cell names this execution: a refcursor parameter's text (TextDatumGetCString, as
/// fetch_cursor_param_value reads it), else the execution's `CurrentOfExpr`'s name. A name that is not UTF-8 names no
/// portal: execCurrentOf's 34000 with its bytes verbatim.
/// # Safety
/// `k` is a non-null cursor word: a live `text` varlena when `by_param`, else the address of a live `CurrentOfExpr`.
pub(crate) unsafe fn cursor_of<'m>(mcx: mcx::Mcx<'m>, k: Word, by_param: bool) -> PgResult<&'m str> {
    if !by_param {
        let c = &*(k as usize as *const types_nodes::primnodes::CurrentOfExpr<'m>);
        return c.cursor_name.ok_or_else(|| crate::pg_err("gravity: a CURRENT OF word without its cursor name"));
    }
    use types_tuple::varatt::{varatt_is_1b, varatt_is_1b_e, varatt_is_4b_u, varsize_any};
    let p = k as usize as *const u8;
    let img = core::slice::from_raw_parts(p, varsize_any(p));
    // a plain image as it is; a compressed or external one detoasted into the execution's context
    let plain = varatt_is_4b_u(p) || (varatt_is_1b(p) && !varatt_is_1b_e(p));
    let flat: &'m [u8] = if plain { img } else { detoast_seams::detoast_attr::call(mcx, img)?.leak() };
    let bytes = if varatt_is_1b(flat.as_ptr()) { &flat[1..] } else { &flat[4..] };
    core::str::from_utf8(bytes).map_err(|_| {
        let mut msg = Vec::with_capacity(bytes.len() + 24); // alloc: Cold
        msg.extend_from_slice(b"cursor \"");
        msg.extend_from_slice(bytes);
        msg.extend_from_slice(b"\" does not exist");
        PgError::error_raw_message(msg).with_sqlstate(ERRCODE_UNDEFINED_CURSOR).into()
    })
}
