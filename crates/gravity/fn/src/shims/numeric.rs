//! Hand adapters over the numeric bodies: the argument-returning `larger` / `smaller`, and the
//! text forms whose wrapper keeps a retained buffer in its `FmgrInfo`.

use core::cell::RefCell;

use adt_numeric::Num;

use crate::{fail, null_ret, region_copy, region_cstring, region_payload, ErrSlot, Region, Ret, View};

pub extern "C-unwind" fn numeric_larger(_e: *mut ErrSlot, r: *mut Region, a0: View, a1: View) -> Ret {
    let pick = if adt_numeric::cmp_numerics(Num::from_payload(a0.bytes()), Num::from_payload(a1.bytes())) > 0 { a0 } else { a1 };
    region_payload(r, pick.bytes())
}

pub extern "C-unwind" fn numeric_smaller(_e: *mut ErrSlot, r: *mut Region, a0: View, a1: View) -> Ret {
    let pick = if adt_numeric::cmp_numerics(Num::from_payload(a0.bytes()), Num::from_payload(a1.bytes())) < 0 { a0 } else { a1 };
    region_payload(r, pick.bytes())
}

thread_local! {
    static OUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) }; // alloc: Node
}

pub fn with_out_buf(f: impl FnOnce(&mut Vec<u8>) -> Ret) -> Ret {
    OUT.with(|b| {
        let mut b = b.borrow_mut();
        b.clear();
        f(&mut b)
    })
}

pub extern "C-unwind" fn numeric_out(_e: *mut ErrSlot, r: *mut Region, a0: View) -> Ret {
    with_out_buf(|buf| {
        adt_numeric::numeric_out_into(Num::from_payload(a0.bytes()), buf);
        region_cstring(r, buf)
    })
}

pub extern "C-unwind" fn numeric_in(e: *mut ErrSlot, r: *mut Region, a0: View, _a1: i64, a2: i64) -> Ret {
    match adt_numeric::io::numeric_in_bytes(a0.bytes(), a2 as i32, None) {
        Ok(Some(img)) => region_copy(r, img.as_bytes()),
        Ok(None) => null_ret(),
        Err(err) => fail(e, err),
    }
}
