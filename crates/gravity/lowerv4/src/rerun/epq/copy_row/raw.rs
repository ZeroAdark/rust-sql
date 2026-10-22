//! The copy leg's raw read (R6's raw home): `copy_field`, the host entry the recheck's IR calls to read one field of a
//! ROW_MARK_COPY mark's stored record image in place, moved unchanged from copy_row.rs.
#![allow(unsafe_code)]
use super::COPY_BUF;

gravity_abi::host! {
    /// Field `word & 0xffff` (type `word >> 16`) of the record image `img` into `out[0]`: 1 a value, 2 NULL, 0 an error
    /// (stashed for the raise). The image is read in place when plain (a 4-byte header: any width, the wide rows); a
    /// short-header one (the build's packed copy of a record under 127 bytes) is rebuilt plain in `buf`, the caller's
    /// COPY_BUF-byte stack slot, live through the row's push into the recheck's build (which copies its by-reference
    /// fields): no state outlives the call's frame, none is shared between sessions. A field stored compressed inside the
    /// record (heap_copy_tuple_as_datum keeps an inline compressed value) is handed out in place, as PostgreSQL's
    /// GetAttributeByNum does: its reader detoasts it. A compressed or external record image (never one: the whole-row
    /// record is formed plain and external fields are flattened into it) is an error by name, never truncated.
    pub(super) unsafe fn copy_field(img: u64, word: u64, out: u64, buf: u64) -> i64 {
        // the host ABI takes every pointer as a 64-bit word (gravity_abi::host; wasm32: the word, not an i32)
        let out = out as *mut u64;
        let buf = buf as *mut u8;
        // SAFETY: `img` is the stored row's live varlena (marks.rs `store`; the stopped node keeps it through the recheck);
        // `buf` is the caller's COPY_BUF-byte slot.
        let img = unsafe {
            let h = *(img as usize as *const u8);
            if gravity_expr::views::varlena_is_plain(img as usize as *const u8) {
                Ok(img)
            } else if h & 1 == 1 && h != 1 && usize::from(h >> 1) + 3 <= COPY_BUF {
                gravity_expr::views::image_view(gravity_fn::Datum::from_usize(img as usize), -1, &mut gravity_fn::Region::over(core::slice::from_raw_parts_mut(buf, COPY_BUF))).map(|v| v.ptr as u64)
            } else {
                // unreachable: the wholerow record is formed plain (heap_form_tuple, external fields flattened) and the build
                // copies it as is; a detoast here would need a buffer outliving this call
                Err(Box::new(gravity_fn::PgError::error("gravity: D732: a compressed or external stored row image (EvalPlanQual's COPY mark)"))) // alloc: Cold
            }
        };
        let img = match img {
            Ok(i) => i,
            Err(e) => {
                gravity_fn::stash_error(e);
                return 0;
            }
        };
        match gravity_expr::nodes::tuple::field_of(img as usize as *const u8, word as u16 as i16, (word >> 16) as u32, None) {
            Ok(v) if v.isnull => 2,
            Ok(v) => {
                // SAFETY: `out` is the caller's 8-byte stack slot.
                unsafe { *out = v.value.as_i64() as u64 };
                1
            }
            Err(e) => {
                gravity_fn::stash_error(e);
                0
            }
        }
    }
}
