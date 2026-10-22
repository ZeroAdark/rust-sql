//! The DataRow writer's unsafe steps (rule R6, the module's raw home): the varlena payload reads of the fast text and
//! numeric paths, the block framers' raw stores, the node's two held contexts, and the output / send function results
//! read as the cstring / bytea images their contracts promise.
#![allow(unsafe_code)]

use core::ffi::CStr;

use super::*;

/// The payload of a plain inline varlena at `p` (`siphash::varlena`'s decode: the 1-byte header, else the 4-byte header when its compressed bit is clear); None for an external or compressed image.
/// # Safety — `p` is a live varlena image.
#[inline]
pub(super) unsafe fn text_payload(p: *const u8) -> Option<(*const u8, usize)> {
    let b0 = *p;
    if b0 & 1 == 1 {
        if b0 == 1 {
            return None;
        }
        return Some((p.add(1), (b0 >> 1) as usize - 1));
    }
    let w = p.cast::<u32>().read_unaligned();
    if w & 2 != 0 {
        return None;
    }
    Some((p.add(4), (w >> 2) as usize - 4))
}

/// The payload of the plain inline varlena at `p` as a slice (`text_payload`); None for an external or compressed image.
/// # Safety — `p` is a live varlena image.
#[inline]
pub(super) unsafe fn payload<'a>(p: *const u8) -> Option<&'a [u8]> {
    let (d, n) = text_payload(p)?;
    Some(core::slice::from_raw_parts(d, n))
}

/// The non-null numeric datum `v` of a row being sent, rendered into `out` when the fast path takes it (`put_numeric`).
#[inline]
pub(super) fn numeric_text(v: u64, out: &mut [u8]) -> Option<usize> {
    // SAFETY: a non-null numeric datum points at a live image.
    put_numeric(unsafe { payload(v as usize as *const u8) }?, out)
}

/// A text output function's result: its cstring's bytes.
#[inline]
pub(super) fn cstring_bytes<'a>(out: Datum) -> &'a [u8] {
    // SAFETY: text output fns return a NUL-terminated cstring datum (the contract C's DatumGetCString trusts).
    unsafe { CStr::from_ptr(out.as_usize() as *const core::ffi::c_char) }.to_bytes()
}

/// A send function's result as its bytea image.
#[inline]
pub(super) fn bytea_image<'a>(out: Datum) -> PackedVarlena<'a> {
    // SAFETY: send fns return an untoasted bytea image (C's DatumGetByteaP).
    unsafe { PackedVarlena::from_ptr(out.as_usize() as *const u8) }
}

/// `frame_rows` for a format with `FAST_TEXT` / `FAST_NUMERIC` columns: the block's frame bytes measured first (a text cell's payload length, a numeric's
/// `NUM_MAX`), then written; false when a text cell is compressed or external or a numeric cell is not the fast path's (the caller sends the block
/// row by row through the output function), `out` untouched.
pub fn frame_rows_text(out: &mut Vec<u8>, fmt: &WireFormat, values: &[u64], isnull: &[u8], rows: usize) -> bool {
    let natts = fmt.natts as usize;
    assert!(values.len() >= rows * natts && isnull.len() >= rows * natts);
    let mut total = 0usize;
    for r in 0..rows {
        total += 7;
        for i in 0..natts {
            let cell = r * natts + i;
            let kind = fmt.kinds[i];
            if isnull[cell] != 0 {
                total += 4;
            } else if kind == FAST_TEXT {
                // SAFETY: a non-null text datum of a pushed block points at a live image (the out region's copy).
                let Some((_, n)) = (unsafe { text_payload(values[cell] as usize as *const u8) }) else { return false };
                total += 4 + n;
            } else if kind == FAST_NUMERIC {
                // SAFETY: as a text cell's: a live numeric image.
                if unsafe { payload(values[cell] as usize as *const u8) }.and_then(num_head).is_none() {
                    return false;
                }
                total += 4 + NUM_MAX;
            } else {
                total += 4 + fast_max(kind);
            }
        }
    }
    out.reserve(total); // alloc: Exec charged by the caller's charge on `out` (WireNode::send_rows: keep_frame fits frame_charge, the session's, to the frame; exec legs: the framed ChargedVec, its room reserved first)
    let base = out.as_mut_ptr();
    let mut len = out.len();
    // SAFETY: every store lands within the reserved `total` bytes past `len`: a text cell writes exactly its measured payload, every other cell at most its kind's width.
    unsafe {
        for r in 0..rows {
            let start = len;
            *base.add(len) = PQMSG_DATA_ROW;
            len += 5;
            core::ptr::copy_nonoverlapping((natts as u16).to_be_bytes().as_ptr(), base.add(len), 2);
            len += 2;
            for i in 0..natts {
                let cell = r * natts + i;
                if *isnull.get_unchecked(cell) != 0 {
                    core::ptr::copy_nonoverlapping((-1i32).to_be_bytes().as_ptr(), base.add(len), 4);
                    len += 4;
                    continue;
                }
                let v = *values.get_unchecked(cell);
                let kind = *fmt.kinds.get_unchecked(i);
                let n = if kind == FAST_TEXT {
                    let (p, n) = text_payload(v as usize as *const u8).unwrap_unchecked();
                    core::ptr::copy_nonoverlapping(p, base.add(len + 4), n);
                    n
                } else if kind == FAST_NUMERIC {
                    // measured above: the fast path takes every numeric cell of the block, NUM_MAX bytes reserved for each
                    put_numeric(payload(v as usize as *const u8).unwrap_unchecked(), core::slice::from_raw_parts_mut(base.add(len + 4), NUM_MAX)).unwrap_unchecked()
                } else {
                    let text = core::slice::from_raw_parts_mut(base.add(len + 4), FAST_MAX);
                    put_fast(kind, v, text)
                };
                core::ptr::copy_nonoverlapping((n as u32).to_be_bytes().as_ptr(), base.add(len), 4);
                len += 4 + n;
            }
            core::ptr::copy_nonoverlapping(((len - start - 1) as u32).to_be_bytes().as_ptr(), base.add(start + 1), 4);
        }
        out.set_len(len);
    }
    true
}

/// `rows` DataRow messages for `values` / `isnull` (row-major, `fmt.natts` per row) appended to `out`: byte for byte what `WireNode::send_row` writes
/// for text-format integer columns (`'D'`, the int32 length, the int16 column count, then `-1` or the digits' length and the digits). One reserve for
/// the block's worst case, then raw stores: no per-value capacity checks and no staging array between the formatter and the frame.
pub fn frame_rows(out: &mut Vec<u8>, fmt: &WireFormat, values: &[u64], isnull: &[u8], rows: usize) {
    let natts = fmt.natts as usize;
    assert!(values.len() >= rows * natts && isnull.len() >= rows * natts);
    debug_assert!(!fmt.kinds[..natts].iter().any(|k| matches!(*k, FAST_TEXT | FAST_NUMERIC)), "a text or numeric column frames through frame_rows_text");
    out.reserve(rows * frame_row_max(fmt)); // alloc: Exec charged by the caller's charge on `out` (WireNode::send_rows: keep_frame fits frame_charge, the session's, to the frame; exec legs: the framed ChargedVec, its room reserved first)
    let base = out.as_mut_ptr();
    let mut len = out.len();
    // SAFETY: every store lands within the reserved `rows * frame_row_max` bytes past `len`: each row writes at most that row's
    // maximum, and the length a formatter returns is at most the width `fast_max` counts for its kind (the text slice is `FAST_MAX` wide).
    unsafe {
        for r in 0..rows {
            let start = len;
            *base.add(len) = PQMSG_DATA_ROW;
            len += 5;
            core::ptr::copy_nonoverlapping((natts as u16).to_be_bytes().as_ptr(), base.add(len), 2);
            len += 2;
            for i in 0..natts {
                let cell = r * natts + i;
                if *isnull.get_unchecked(cell) != 0 {
                    core::ptr::copy_nonoverlapping((-1i32).to_be_bytes().as_ptr(), base.add(len), 4);
                    len += 4;
                    continue;
                }
                let v = *values.get_unchecked(cell);
                let text = core::slice::from_raw_parts_mut(base.add(len + 4), FAST_MAX);
                let n = put_fast(*fmt.kinds.get_unchecked(i), v, text);
                core::ptr::copy_nonoverlapping((n as u32).to_be_bytes().as_ptr(), base.add(len), 4);
                len += 4 + n;
            }
            core::ptr::copy_nonoverlapping(((len - start - 1) as u32).to_be_bytes().as_ptr(), base.add(start + 1), 4);
        }
        out.set_len(len);
    }
}

impl WireNode {
    pub(super) fn mcx(&self) -> Mcx<'static> {
        // SAFETY: `node_ctx` outlives every object placed in it: they are
        // fields of the same struct, declared before it, so they drop first.
        unsafe { self.node_ctx.held() }
    }

    pub(super) fn desc_mcx(&self) -> Mcx<'static> {
        // SAFETY: as `mcx`; `rowdesc` is dropped before every reset.
        unsafe { self.desc_ctx.held() }
    }
}
