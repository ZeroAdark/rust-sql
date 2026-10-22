//! Hand adapters over the date/time output bodies, which write into a caller buffer (the
//! wrapper's retained scratch); the shim gives them a stack buffer and copies into the region.

use crate::{fail, region_copy, region_cstring, ErrSlot, Region, Ret, View};

pub extern "C-unwind" fn timestamp_out(e: *mut ErrSlot, r: *mut Region, a0: i64) -> Ret {
    let mut buf: adt_timestamp::TsBuf = [0; adt_datetime::MAXDATELEN + 1];
    match adt_timestamp::timestamp_out(a0, &mut buf) {
        Ok(len) => region_cstring(r, &buf[..len]),
        Err(err) => fail(e, err),
    }
}

pub extern "C-unwind" fn timestamptz_out(e: *mut ErrSlot, r: *mut Region, a0: i64) -> Ret {
    let mut buf: adt_timestamp::TsBuf = [0; adt_datetime::MAXDATELEN + 1];
    match adt_timestamp::timestamptz_out(a0, &mut buf) {
        Ok(len) => region_cstring(r, &buf[..len]),
        Err(err) => fail(e, err),
    }
}

pub extern "C-unwind" fn date_out(_e: *mut ErrSlot, r: *mut Region, a0: i64) -> Ret {
    let mut buf: adt_date::DateBuf = [0; adt_datetime::MAXDATELEN + 1];
    let len = adt_date::date_out(a0 as i32, &mut buf);
    region_cstring(r, &buf[..len])
}

pub extern "C-unwind" fn time_out(_e: *mut ErrSlot, r: *mut Region, a0: i64) -> Ret {
    let mut buf: adt_date::DateBuf = [0; adt_datetime::MAXDATELEN + 1];
    let len = adt_date::time_out(a0, &mut buf);
    region_cstring(r, &buf[..len])
}

pub extern "C-unwind" fn interval_out(_e: *mut ErrSlot, r: *mut Region, a0: View) -> Ret {
    let mut buf: adt_timestamp::TsBuf = [0; adt_datetime::MAXDATELEN + 1];
    let len = adt_timestamp::interval::interval_out(&a0.interval(), &mut buf);
    region_cstring(r, &buf[..len])
}

fn timetz(v: View) -> adt_date::TimeTzADT {
    let b = v.fixed::<12>();
    adt_date::TimeTzADT { time: i64::from_ne_bytes(b[..8].try_into().unwrap()), zone: i32::from_ne_bytes(b[8..].try_into().unwrap()) }
}

pub extern "C-unwind" fn timetz_larger(_e: *mut ErrSlot, r: *mut Region, a0: View, a1: View) -> Ret {
    let pick = if adt_date::timetz_cmp_internal(&timetz(a0), &timetz(a1)) > 0 { a0 } else { a1 };
    region_copy(r, pick.fixed::<12>())
}

pub extern "C-unwind" fn timetz_smaller(_e: *mut ErrSlot, r: *mut Region, a0: View, a1: View) -> Ret {
    let pick = if adt_date::timetz_cmp_internal(&timetz(a0), &timetz(a1)) < 0 { a0 } else { a1 };
    region_copy(r, pick.fixed::<12>())
}

/// `now()` / `transaction_timestamp()`: the transaction's start, read as `fc_now` reads it (a pool claim arms the leader's,
/// `SessionCarry`). A by-value shim of no argument: the emitted code calls it directly.
pub extern "C-unwind" fn now(_e: *mut ErrSlot) -> Ret {
    crate::ok(xact::GetCurrentTransactionStartTimestamp())
}

/// `statement_timestamp()`: the statement's start, as `fc_statement_timestamp` reads it (carried to a pool claim too).
pub extern "C-unwind" fn statement_timestamp(_e: *mut ErrSlot) -> Ret {
    crate::ok(xact::GetCurrentStatementStartTimestamp())
}
