//! M2's `tz_fixed_offset` host entry (kernels host_abi v4 m2.rs, M2-FRAME-EXPR's expr.fusion): the session zone's constant
//! UTC offset (east positive) per call, `i64::MIN` for a zone with daylight-saving rules or none.
use adt_datetime::tz;
use gravity_kernels::ops::scan::entries::tz_fixed_offset;

fn call() -> i64 {
    // SAFETY: the entry reads session state only; its context word is unused.
    unsafe { tz_fixed_offset(0) as i64 }
}

#[test]
fn a_fixed_zone_gives_its_offset_and_a_dst_zone_gives_min() {
    let system = std::path::Path::new("/usr/share/zoneinfo/America/New_York").exists();
    if system {
        std::env::set_var("PGRUST_TZDIR", "/usr/share/zoneinfo");
    }
    pgtz::init_seams();
    xact_seams::get_current_sub_transaction_id::set(|| 1);
    tz::set_session_timezone(None);
    assert_eq!(call(), i64::MIN, "no session zone");
    tz::set_session_timezone(pgtz::pg_tzset(b"UTC").unwrap().or(pgtz::pg_tzset(b"GMT").unwrap()));
    assert_eq!(call(), 0, "UTC");
    // SET TimeZone = '+05:30' is POSIX: 5 h 30 west of Greenwich
    tz::set_session_timezone(pgtz::pg_tzset_offset(5 * 3600 + 1800).unwrap());
    assert_eq!(call(), -(5 * 3600 + 1800), "'+05:30' (POSIX sign)");
    tz::set_session_timezone(pgtz::pg_tzset_offset(-(5 * 3600 + 1800)).unwrap());
    assert_eq!(call(), 5 * 3600 + 1800, "a zone east of Greenwich");
    // America/New_York from the system tz database, else its rules spelled as a POSIX string
    let ny: &[u8] = if system { b"America/New_York" } else { b"EST5EDT,M3.2.0,M11.1.0" };
    tz::set_session_timezone(pgtz::pg_tzset(ny).unwrap());
    assert_eq!(call(), i64::MIN, "a zone with daylight-saving time");
    // SET TimeZone between two calls: the second sees the new zone
    tz::set_session_timezone(pgtz::pg_tzset(b"GMT").unwrap());
    assert_eq!(call(), 0, "SET TimeZone between two calls");
    // B2-31: `tz_offset_at`, the zone's offset at one instant (`timestamp2tm`'s `pg_localtime`): America/New_York's
    // standard and daylight offsets on either side of the 2026-03-08 07:00 UTC change, a fixed zone's constant, `i64::MIN`
    // with no zone (one test: the seams install once per process)
    tz::set_session_timezone(None);
    assert_eq!(at(821_793_600_000_000), i64::MIN, "no session zone");
    tz::set_session_timezone(pgtz::pg_tzset(ny).unwrap());
    assert_eq!(at(821_793_600_000_000), -5 * 3600, "2026-01-15 12:00 UTC: EST");
    assert_eq!(at(837_432_000_000_000), -4 * 3600, "2026-07-15 12:00 UTC: EDT");
    assert_eq!(at(826_268_399_000_000), -5 * 3600, "one second before the 2026-03-08 change");
    assert_eq!(at(826_268_400_000_000), -4 * 3600, "at the change");
    assert_eq!(at(826_268_399_999_999), -5 * 3600, "a sub-second instant reads its whole second");
    tz::set_session_timezone(pgtz::pg_tzset_offset(-(5 * 3600 + 1800)).unwrap());
    assert_eq!(at(837_432_000_000_000), 5 * 3600 + 1800, "a fixed zone east of Greenwich");
}

fn at(ts: i64) -> i64 {
    // SAFETY: the entry reads session state only; its context word is unused.
    unsafe { gravity_kernels::ops::scan::entries::tz_offset_at(0, ts as u64) as i64 }
}
