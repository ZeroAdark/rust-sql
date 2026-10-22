//! Hand adapters over the network bodies: the argument-returning `larger` / `smaller`.

use adt_network::{network_cmp_internal, InetRef};

use crate::{region_copy, ErrSlot, Region, Ret, View};

fn inet(v: View) -> InetRef<'static> {
    InetRef::from_payload(&v.bytes()[4..])
}

pub extern "C-unwind" fn network_larger(_e: *mut ErrSlot, r: *mut Region, a0: View, a1: View) -> Ret {
    let pick = if network_cmp_internal(inet(a0), inet(a1)) > 0 { a0 } else { a1 };
    region_copy(r, pick.bytes())
}

pub extern "C-unwind" fn network_smaller(_e: *mut ErrSlot, r: *mut Region, a0: View, a1: View) -> Ret {
    let pick = if network_cmp_internal(inet(a0), inet(a1)) < 0 { a0 } else { a1 };
    region_copy(r, pick.bytes())
}
