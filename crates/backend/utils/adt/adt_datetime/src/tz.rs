//! Boundary to the tz engine (backend/timezone: localtime/pgtz crates) plus
//! the datetime.c helpers that sit directly on it (DetermineTimeZoneOffset
//! family, TimeZoneAbbrevIsKnown). The engine's PgTm is POSIX-convention
//! (tm_year-1900, 0-based tm_mon) and pg_localtime/pg_gmtime results keep
//! that convention, exactly as C's callers expect; timestamp2tm converts.

use core::cell::Cell;

use pgsync::Mutex;

use crate::calendar::date2j;
use crate::consts::{
    fsec_t, pg_tm, DateTimeErrorExtra, DateTkn, DTZ, DYNTZ, IS_VALID_JULIAN, MINS_PER_HOUR,
    SECS_PER_DAY, SECS_PER_MINUTE, TOKMAXLEN, TZ, UNIX_EPOCH_JDATE,
};

pub use localtime::{PgTz, PgTzRef};
pub use pgtz::{
    log_timezone, pg_timezone_initialize, pg_tzset, pg_tzset_offset, session_timezone,
    set_log_timezone, set_session_timezone, with_session_timezone,
};

pub use localtime::TZ_STRLEN_MAX;
use localtime::NextDstBoundary;
use types_error::PgResult;

/// [`with_session_timezone`] for a reader that needs a zone (every backend has one after `pg_timezone_initialize`):
/// `missing` is the panic text when there is none. The per-datum date / timestamp / timestamptz readers take this,
/// never a `session_timezone()` handle (a clone + drop per row on the Arc all pool workers share).
#[inline]
pub fn with_required_session_timezone<R>(missing: &'static str, f: impl FnOnce(&PgTz) -> R) -> R {
    with_session_timezone(|z| f(z.unwrap_or_else(|| no_session_timezone(missing))))
}

#[cold]
#[inline(never)]
fn no_session_timezone(missing: &'static str) -> ! {
    panic!("{missing}")
}

pub fn pg_tz_acceptable(tz: &PgTz) -> bool {
    localtime::pg_tz_acceptable(tz)
}

pub fn pg_get_timezone_name(tz: &PgTz) -> Option<&str> {
    core::str::from_utf8(localtime::pg_get_timezone_name(tz)).ok()
}

/// datetime.c `DetermineTimeZoneOffset`: GMT offset and DST status for the
/// datetime-convention y/m/d/h/m/s in `tm` under `tzp`; sets tm_isdst.
/// Out-of-range dates yield offset 0 / isdst 0 (no error here).
#[allow(non_snake_case)]
pub fn DetermineTimeZoneOffset(tm: &mut pg_tm, tzp: &PgTz) -> i32 {
    determine_time_zone_offset_internal(tm, tzp).0
}

// datetime.c DetermineTimeZoneOffsetInternal: also returns the UTC time
// imputed to the date/time (0 on overflow). DST boundaries assumed >= 48
// hours apart, zone offsets < 24h, so back up 24h and find the next boundary.
fn determine_time_zone_offset_internal(tm: &mut pg_tm, tzp: &PgTz) -> (i32, i64) {
    #[cold]
    fn overflow(tm: &mut pg_tm) -> (i32, i64) {
        // Given date is out of range, so assume UTC.
        tm.tm_isdst = 0;
        (0, 0)
    }

    if !IS_VALID_JULIAN(tm.tm_year, tm.tm_mon, tm.tm_mday) {
        return overflow(tm);
    }
    let date = date2j(tm.tm_year, tm.tm_mon, tm.tm_mday) - UNIX_EPOCH_JDATE;

    let Some(day) = (date as i64).checked_mul(SECS_PER_DAY as i64) else {
        return overflow(tm);
    };
    let sec = tm.tm_sec as i64
        + (tm.tm_min as i64 + tm.tm_hour as i64 * MINS_PER_HOUR as i64) * SECS_PER_MINUTE as i64;
    let mytime = day.wrapping_add(sec);
    // since sec >= 0, overflow could only be from +day to -mytime
    if mytime < 0 && day > 0 {
        return overflow(tm);
    }

    let prevtime = mytime.wrapping_sub(SECS_PER_DAY as i64);
    if mytime < 0 && prevtime > 0 {
        return overflow(tm);
    }

    let b = match localtime::pg_next_dst_boundary(prevtime, tzp) {
        NextDstBoundary::Overflow => return overflow(tm),
        NextDstBoundary::NoTransition {
            before_gmtoff,
            before_isdst,
        } => {
            // Non-DST zone, life is simple.
            tm.tm_isdst = before_isdst;
            return (-(before_gmtoff as i32), mytime - before_gmtoff);
        }
        NextDstBoundary::Boundary(b) => b,
    };

    let beforetime = mytime.wrapping_sub(b.before_gmtoff);
    if (b.before_gmtoff > 0 && mytime < 0 && beforetime > 0)
        || (b.before_gmtoff <= 0 && mytime > 0 && beforetime < 0)
    {
        return overflow(tm);
    }
    let aftertime = mytime.wrapping_sub(b.after_gmtoff);
    if (b.after_gmtoff > 0 && mytime < 0 && aftertime > 0)
        || (b.after_gmtoff <= 0 && mytime > 0 && aftertime < 0)
    {
        return overflow(tm);
    }

    // The boundary instant itself counts as after the transition.
    if beforetime < b.boundary && aftertime < b.boundary {
        tm.tm_isdst = b.before_isdst;
        return (-(b.before_gmtoff as i32), beforetime);
    }
    if beforetime > b.boundary && aftertime >= b.boundary {
        tm.tm_isdst = b.after_isdst;
        return (-(b.after_gmtoff as i32), aftertime);
    }

    // Invalid or ambiguous time at a transition: spring-forward prefers the
    // "before" interpretation, fall-back prefers "after" (not "standard
    // time" — Europe/Moscow Oct 2014, Europe/Dublin).
    if beforetime > aftertime {
        tm.tm_isdst = b.before_isdst;
        return (-(b.before_gmtoff as i32), beforetime);
    }
    tm.tm_isdst = b.after_isdst;
    (-(b.after_gmtoff as i32), aftertime)
}

fn upcase_abbrev(abbr: &[u8]) -> ([u8; TZ_STRLEN_MAX + 1], usize) {
    let mut up = [0u8; TZ_STRLEN_MAX + 1];
    let n = abbr.len().min(TZ_STRLEN_MAX);
    for (dst, src) in up.iter_mut().zip(abbr[..n].iter()) {
        *dst = src.to_ascii_uppercase();
    }
    (up, n)
}

/// datetime.c `DetermineTimeZoneAbbrevOffset`: offset/DST flag for a dynamic
/// abbreviation at the local time in `tm`; a std/dst abbreviation forces its
/// own offset even when the zone was then in the other mode; otherwise falls
/// back to DetermineTimeZoneOffset's answers.
#[allow(non_snake_case)]
pub fn DetermineTimeZoneAbbrevOffset(tm: &mut pg_tm, abbr: &[u8], tzp: &PgTz) -> i32 {
    let (zone_offset, t) = determine_time_zone_offset_internal(tm, tzp);

    let (up, n) = upcase_abbrev(abbr);
    if let Some((gmtoff, isdst)) = localtime::pg_interpret_timezone_abbrev(&up[..n], t, tzp) {
        tm.tm_isdst = isdst;
        // Change sign to agree with DetermineTimeZoneOffset().
        return -(gmtoff as i32);
    }

    zone_offset
}

#[allow(non_snake_case)]
pub fn pg_get_timezone_offset(tzp: &PgTz, gmtoff: &mut i64) -> bool {
    match localtime::pg_get_timezone_offset(tzp) {
        Some(off) => {
            *gmtoff = off;
            true
        }
        None => false,
    }
}

/// datetime.c `TimeZoneAbbrevIsKnown` probe of `session_timezone`: returns
/// (isfixed, offset, isdst) with the sign flipped to the
/// DetermineTimeZoneOffset convention (the caller flips once more to match
/// zoneabbrevtbl's convention).
pub fn session_tz_abbrev_probe(lowtoken: &[u8]) -> Option<(bool, i32, i32)> {
    let (up, n) = upcase_abbrev(lowtoken);
    let (isfixed, gmtoff, isdst) =
        with_session_timezone(|tz| localtime::pg_timezone_abbrev_is_known(&up[..n], tz?))?;
    Some((isfixed, -(gmtoff as i32), isdst))
}

// pg_tm.tm_zone is `&'static str` across the adt crates while the zone is a
// session-cache handle: intern the abbreviation text (bytes, not tz states).
// Every breakdown under a zone (timestamp2tm: timestamptz output, ::date,
// date_trunc, + interval, extract) interns, per datum, on every pool worker at
// once. So a text seen before is found in INTERN_FAST with plain loads (no
// read-modify-write: the slots' lines stay shared in every core's cache); only
// a new text takes the lock (a lock per datum was the next shared line after
// the session zone's refcount: claimsbench t01, 4 workers, 43 % of CPU in
// __aarch64_cas4_acq / swp4_rel / lock_contended). A process meets a few dozen
// abbreviations; past INTERN_SLOTS the rest are found under the lock as before.
const INTERN_SLOTS: usize = 64;
static INTERN_FAST: [pgsync::OnceLock<&'static str>; INTERN_SLOTS] =
    [const { pgsync::OnceLock::new() }; INTERN_SLOTS];

fn intern_abbrev(abbr: Option<&str>) -> Option<&'static str> {
    let abbr = abbr?;
    // slots fill in order under the lock: the first empty one ends the published run
    for slot in &INTERN_FAST {
        match slot.get() {
            Some(&s) if s == abbr => return Some(s),
            Some(_) => {}
            None => break,
        }
    }
    Some(intern_locked(abbr))
}

#[cold]
#[inline(never)]
fn intern_locked(abbr: &str) -> &'static str {
    use std::collections::BTreeSet;
    static INTERNED: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());
    let mut set = ::pgsync::lock(&INTERNED);
    let s: &'static str = match set.get(abbr) {
        Some(&s) => s,
        None => {
            let leaked: &'static str = Box::leak(abbr.to_owned().into_boxed_str());
            set.insert(leaked);
            leaked
        }
    };
    // Publish it unless a slot has it already (a reader that scanned before
    // the publish lands here too); only this lock's holder sets a slot.
    for slot in &INTERN_FAST {
        match slot.get() {
            Some(&p) if p == s => break,
            Some(_) => {}
            None => {
                let _ = slot.set(s);
                break;
            }
        }
    }
    s
}

fn convert(tx: localtime::PgTm<'_>) -> pg_tm {
    pg_tm {
        tm_sec: tx.tm_sec,
        tm_min: tx.tm_min,
        tm_hour: tx.tm_hour,
        tm_mday: tx.tm_mday,
        tm_mon: tx.tm_mon,
        tm_year: tx.tm_year,
        tm_wday: tx.tm_wday,
        tm_yday: tx.tm_yday,
        tm_isdst: tx.tm_isdst,
        tm_gmtoff: tx.tm_gmtoff,
        tm_zone: intern_abbrev(tx.tm_zone),
    }
}

/// POSIX-convention result (see the engine's PgTm doc): tm_year is year-1900,
/// tm_mon 0-based — converted at the timestamp2tm boundary, not here.
#[allow(non_snake_case)]
pub fn pg_localtime(t: i64, tzp: &PgTz) -> Option<pg_tm> {
    localtime::pg_localtime(t, tzp).map(convert)
}

pub fn pg_gmtime(t: i64) -> Option<pg_tm> {
    localtime::pg_gmtime(t).map(convert)
}

/// tzparser's tzEntry, minus the source-location fields ConvertTimeZoneAbbrevs
/// ignores. `abbrev` must be downcased and the slice sorted by strcmp order.
pub struct TzEntry<'a> {
    pub abbrev: &'a [u8],
    pub zone: Option<&'a [u8]>,
    pub offset: i32,
    pub is_dst: bool,
}

pub struct DynamicZoneAbbrev {
    // C: `pg_tz *tz`, resolved on first use; the slot's own handle outlives
    // the resolving session's cache (the table is process-shared).
    tz: Mutex<Option<PgTzRef>>,
    zone: &'static [u8],
}

/// C `zoneabbrevtbl` (installed by the `timezone_abbreviations` GUC via
/// `InstallTimeZoneAbbrevs`; NULL until then). DYNTZ tokens hold an index
/// into `dynamic` (C: a byte offset into the same guc_malloc chunk).
pub struct ZoneAbbrevTable {
    pub abbrevs: &'static [DateTkn],
    dynamic: &'static [DynamicZoneAbbrev],
}

impl ZoneAbbrevTable {
    /// Differential-test accessor (tzfam_diff): the zone name behind a
    /// DYNTZ token's `value` index. C stores a byte offset into the same
    /// guc_malloc chunk; here `value` indexes `dynamic` directly.
    pub fn dynamic_zone(&self, value: i32) -> &[u8] {
        self.dynamic[value as usize].zone
    }
}

thread_local! {
    static ZONEABBREVTBL: Cell<Option<&'static ZoneAbbrevTable>> = const { Cell::new(None) };
}

#[inline]
pub fn zoneabbrevtbl() -> Option<&'static ZoneAbbrevTable> {
    ZONEABBREVTBL.with(Cell::get)
}

// DIVERGENCE: C guc_mallocs one chunk freed with the superseded GUC extra;
// here the table is permanent, so tzparser interns one per file name.
#[allow(non_snake_case)]
pub fn ConvertTimeZoneAbbrevs(abbrevs: &[TzEntry<'_>]) -> &'static ZoneAbbrevTable {
    let mut tokens: Vec<DateTkn> = Vec::new();
    let _ = tokens.try_reserve_exact(abbrevs.len());
    let mut dynamic: Vec<DynamicZoneAbbrev> = Vec::new();
    for abbr in abbrevs {
        let mut token = [0u8; TOKMAXLEN + 1];
        let n = abbr.abbrev.len().min(TOKMAXLEN);
        token[..n].copy_from_slice(&abbr.abbrev[..n]);
        let (typ, value) = match abbr.zone {
            Some(zone) => {
                dynamic.push(DynamicZoneAbbrev {
                    tz: Mutex::new(None),
                    zone: Box::leak(Box::<[u8]>::from(zone)),
                });
                (DYNTZ, (dynamic.len() - 1) as i32)
            }
            None => (if abbr.is_dst { DTZ } else { TZ }, abbr.offset),
        };
        tokens.push(DateTkn { token, typ: typ as i8, value });
    }
    debug_assert!(crate::decode::CheckDateTokenTable(&tokens));
    Box::leak(Box::new(ZoneAbbrevTable {
        abbrevs: Box::leak(tokens.into_boxed_slice()),
        dynamic: Box::leak(dynamic.into_boxed_slice()),
    }))
}

#[allow(non_snake_case)]
pub fn InstallTimeZoneAbbrevs(tbl: &'static ZoneAbbrevTable) {
    ZONEABBREVTBL.with(|c| c.set(Some(tbl)));
    crate::decode::ClearTimeZoneAbbrevCache();
}

#[allow(non_snake_case)]
pub fn FetchDynamicTimeZone<'a>(
    tbl: &'a ZoneAbbrevTable,
    tp: &'a DateTkn,
    extra: &mut DateTimeErrorExtra<'a>,
) -> PgResult<Option<PgTzRef>> {
    debug_assert_eq!(tp.typ as i32, DYNTZ);
    let dtza = &tbl.dynamic[tp.value as usize];
    if let Some(tz) = pgsync::lock(&dtza.tz).clone() {
        return Ok(Some(tz));
    }
    match pg_tzset(dtza.zone)? {
        Some(tz) => {
            let mut slot = pgsync::lock(&dtza.tz);
            let tz = slot.get_or_insert(tz).clone();
            Ok(Some(tz))
        }
        None => {
            extra.dtee_timezone = Some(dtza.zone);
            extra.dtee_abbrev = Some(tp.token_bytes());
            Ok(None)
        }
    }
}

/// C `pg_interpret_timezone_abbrev` with the upcase C's callers apply first
/// (DetermineTimeZoneAbbrevOffsetTS needs it from adt_timestamp).
pub fn interpret_timezone_abbrev_at(abbr: &[u8], t: i64, tzp: &PgTz) -> Option<(i64, i32)> {
    let (up, n) = upcase_abbrev(abbr);
    localtime::pg_interpret_timezone_abbrev(&up[..n], t, tzp)
}

fn from_snapshot(s: &timestamp_seams::CurrentTimeUsec, tm: &mut pg_tm) {
    tm.tm_sec = s.tm_sec;
    tm.tm_min = s.tm_min;
    tm.tm_hour = s.tm_hour;
    tm.tm_mday = s.tm_mday;
    tm.tm_mon = s.tm_mon;
    tm.tm_year = s.tm_year;
    tm.tm_wday = s.tm_wday;
    tm.tm_yday = s.tm_yday;
    tm.tm_isdst = s.tm_isdst;
    tm.tm_gmtoff = s.tm_gmtoff;
    tm.tm_zone = s.tm_zone;
}

// datetime.c:423-431 GetCurrentTimeUsec: a transaction start timestamp that
// timestamp2tm rejects is ereport(ERROR, ERRCODE_DATETIME_VALUE_OUT_OF_RANGE,
// "timestamp out of range"); the seam (adt_timestamp::GetCurrentTimeUsec)
// raises exactly that error and the decoders carry it out through their
// PgResult.
#[allow(non_snake_case)]
pub fn GetCurrentDateTime(tm: &mut pg_tm) -> PgResult<()> {
    let s = timestamp_seams::get_current_datetime::call()?;
    from_snapshot(&s, tm);
    Ok(())
}

#[allow(non_snake_case)]
pub fn GetCurrentTimeUsec(
    tm: &mut pg_tm,
    fsec: &mut fsec_t,
    tzp: Option<&mut i32>,
) -> PgResult<()> {
    let s = timestamp_seams::get_current_time_usec::call()?;
    from_snapshot(&s, tm);
    *fsec = s.fsec;
    if let Some(tzp) = tzp {
        *tzp = s.tz;
    }
    Ok(())
}
