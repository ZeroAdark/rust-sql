//! D2992: kernels `zone_cross_bound` turns a cross-unit comparand into the lane's units exactly, as PostgreSQL's
//! date vs timestamp comparator orders them: midnight bounds, a bound inside a day, the infinities, and a date past the
//! timestamp range. (The timestamptz kinds read the session TimeZone; the e2e witness gdict_stable_zone covers them.)
use gravity_kernels::ops::scan::entries::{zone_cross_bound, CROSS_DATE_TS, CROSS_NONE, CROSS_TS_DATE};

const DAY: i64 = 86_400_000_000;
const OPS: [u64; 5] = [0, 1, 2, 3, 4]; // Lt Le Eq Ge Gt

fn bounds(kind: u8, v: i64) -> [Option<i64>; 5] {
    OPS.map(|op| {
        let w = unsafe { zone_cross_bound(kind as u64, op, v as u64) };
        (w != CROSS_NONE).then_some(w as i64)
    })
}

#[test]
fn date_lane_against_timestamp_bounds() {
    let d0 = 9411i64;
    // [<, <=, =, >=, >]: the greatest passing date for < / <=, the least for >= / >, the one for =
    assert_eq!(bounds(CROSS_DATE_TS, d0 * DAY), [Some(d0 - 1), Some(d0), Some(d0), Some(d0), Some(d0 + 1)]);
    assert_eq!(bounds(CROSS_DATE_TS, d0 * DAY + 1), [Some(d0), Some(d0), None, Some(d0 + 1), Some(d0 + 1)]);
    assert_eq!(bounds(CROSS_DATE_TS, d0 * DAY - 1), [Some(d0 - 1), Some(d0 - 1), None, Some(d0), Some(d0)]);
    let (ninf, pinf) = (i32::MIN as i64, i32::MAX as i64);
    // timestamp +infinity: only date +infinity reaches it (the greatest date below it is the last finite one);
    // -infinity: every date does (the least above it is the first finite one)
    assert_eq!(bounds(CROSS_DATE_TS, i64::MAX), [Some(2_145_031_948), Some(pinf), Some(pinf), Some(pinf), None]);
    assert_eq!(bounds(CROSS_DATE_TS, i64::MIN), [None, Some(ninf), Some(ninf), Some(ninf), Some(-2_451_545)]);
    // a bound inside a day: the next midnight
    let late = 300_000_000i64 * 1_000_000;
    assert_eq!(bounds(CROSS_DATE_TS, late)[3], Some((late + DAY - 1) / DAY));
}

#[test]
fn timestamp_lane_against_date_bounds() {
    let d0 = -730i64;
    assert_eq!(bounds(CROSS_TS_DATE, d0), [Some(d0 * DAY - 1), Some(d0 * DAY), Some(d0 * DAY), Some(d0 * DAY), Some(d0 * DAY + 1)]);
    // date +infinity: only timestamp +infinity equals it
    assert_eq!(bounds(CROSS_TS_DATE, i32::MAX as i64), [Some(i64::MAX - 1), Some(i64::MAX), Some(i64::MAX), Some(i64::MAX), None]);
    // a finite date past the timestamp range orders above every finite timestamp, below +infinity
    let far = 106_800_000i64;
    assert_eq!(bounds(CROSS_TS_DATE, far), [Some(i64::MAX - 1), Some(i64::MAX - 1), None, Some(i64::MAX), Some(i64::MAX)]);
    // date -infinity: a timestamp >= it is any; the bound -infinity itself reads as none (only a skip forgone)
    assert_eq!(bounds(CROSS_TS_DATE, i32::MIN as i64)[3], None);
    assert_eq!(bounds(CROSS_TS_DATE, i32::MIN as i64)[4], Some(i64::MIN + 1));
}
