//! `StatementArm`: a thread armed with the statement's stamp gives a C-tier text function the
//! backend's answer (the database default locale and encoding), where a bare thread does not.
mod common;

use datum::Datum;
use gravity_fn::typesem::collation::StatementArm;

const UPPER_TEXT: u32 = 871;
const DEFAULT_COLLATION_OID: u32 = 100;
const PG_UTF8: i32 = 6;
const PG_SQL_ASCII: i32 = 0;

fn varlena_payload(p: *const u8) -> Vec<u8> {
    // SAFETY: p is a varlena the call returned; its header bounds the read.
    unsafe {
        let b0 = *p;
        if b0 & 1 == 1 {
            let len = (b0 >> 1) as usize;
            std::slice::from_raw_parts(p.add(1), len - 1).to_vec()
        } else {
            let w = u32::from_le_bytes([*p, *p.add(1), *p.add(2), *p.add(3)]);
            let len = (w >> 2) as usize;
            std::slice::from_raw_parts(p.add(4), len - 4).to_vec()
        }
    }
}

fn upper_of(s: &str) -> Result<String, String> {
    let img = common::image(s.as_bytes());
    let mut call = gravity_fn::FmgrCall::bind(UPPER_TEXT, 1, DEFAULT_COLLATION_OID).map_err(|e| e.message().to_string())?;
    let r = call.call_site(&[Datum::from_usize(img.as_ptr() as usize)], &[false]).map_err(|e| e.message().to_string())?;
    assert!(!r.isnull);
    Ok(String::from_utf8(varlena_payload(r.value.as_usize() as *const u8)).unwrap())
}

#[test]
fn an_armed_thread_gives_the_backend_answer() {
    common::install_seams();
    mbutils::SetDatabaseEncoding(PG_UTF8 as _).unwrap();
    pg_locale::set_default_locale_builtin_utf8_for_tests();
    let stamp = StatementArm::capture();
    assert_eq!(stamp.encoding(), PG_UTF8);
    let serial = upper_of("caf\u{e9}");
    assert_eq!(serial.as_deref(), Ok("CAF\u{c9}"));

    let (bare, armed) = std::thread::spawn(move || {
        let bare = std::panic::catch_unwind(|| upper_of("caf\u{e9}")).unwrap_or_else(|_| Err("panic".to_string()));
        stamp.apply().unwrap();
        let armed = upper_of("caf\u{e9}");
        assert_eq!(StatementArm::capture(), stamp);
        (bare, armed)
    })
    .join()
    .unwrap();
    assert_ne!(bare, serial, "a bare worker thread has no default locale");
    assert_eq!(armed, serial);
}

/// D351: a standing pool thread also runs parallel query tasks, whose GUC store reset and query-pin bind fire the TimeZone
/// assign hook (`tz::set_session_timezone`) and move the encoding and default locale. The next claim under the SAME stamp
/// re-arms each piece: a remembered "last applied" stamp skipped it, and the thread rendered timestamptz in the task's zone.
#[test]
fn the_same_stamp_rearms_a_thread_a_parallel_task_moved() {
    use adt_datetime::tz;
    common::install_seams();
    mbutils::SetDatabaseEncoding(PG_UTF8 as _).unwrap();
    pg_locale::set_default_locale_builtin_utf8_for_tests();
    let zone = pgtz::pg_tzset_offset(5 * 3600).unwrap().expect("a fixed-offset zone");
    let other = pgtz::pg_tzset(b"GMT").unwrap().expect("GMT");
    assert!(!tz::PgTzRef::ptr_eq(&zone, &other));
    tz::set_session_timezone(Some(zone.clone()));
    let stamp = StatementArm::capture();

    std::thread::spawn(move || {
        stamp.apply().unwrap();
        assert_eq!(StatementArm::capture(), stamp);

        tz::set_session_timezone(Some(other)); // the TimeZone assign hook, as a task's bind_query_pin fires it
        stamp.apply().unwrap();
        assert!(tz::session_timezone().is_some_and(|z| tz::PgTzRef::ptr_eq(&z, &zone)), "the thread's zone is the stamp's");

        mbutils::SetDatabaseEncoding(PG_SQL_ASCII as _).unwrap();
        stamp.apply().unwrap();
        assert_eq!(mbutils::GetDatabaseEncoding() as i32, PG_UTF8, "the thread's encoding is the stamp's");

        pg_locale::set_default_locale_c_for_tests();
        stamp.apply().unwrap();
        assert!(
            pg_locale::default_locale().is_some_and(|l| std::ptr::eq(l, &pg_locale::BUILTIN_C_UTF8_LOCALE)),
            "the thread's default locale is the stamp's"
        );
        assert_eq!(StatementArm::capture(), stamp);
    })
    .join()
    .unwrap();
}

/// D3112: the timezone-abbreviation cache answers a token from this thread's last lookup at that field, and that answer
/// depends on the session zone (an abbreviation the zone itself uses wins over the table). A claim's `apply` that moves
/// the zone drops it, as PG 18's assign_timezone does: an 'IST' cached under GMT (the Default set's Israel +02) read the
/// next claim's IST row under Asia/Kolkata as +02 instead of +05:30.
#[test]
fn a_claim_that_moves_the_zone_drops_cached_abbreviations() {
    use adt_datetime::tz;
    common::install_seams();
    mbutils::SetDatabaseEncoding(PG_UTF8 as _).unwrap();
    pg_locale::set_default_locale_builtin_utf8_for_tests();
    let kolkata = pgtz::pg_tzset(b"Asia/Kolkata").unwrap().expect("Asia/Kolkata");
    let gmt = pgtz::pg_tzset(b"GMT").unwrap().expect("GMT");
    tz::set_session_timezone(Some(kolkata));
    let stamp = StatementArm::capture();

    std::thread::spawn(move || {
        let ist = tz::TzEntry { abbrev: b"ist", zone: None, offset: 7200, is_dst: false };
        tz::InstallTimeZoneAbbrevs(tz::ConvertTimeZoneAbbrevs(&[ist]));
        let at = |s: &str| adt_timestamp::timestamptz_in(s, -1, None).unwrap();
        tz::set_session_timezone(Some(gmt)); // the previous claim's session
        assert_eq!(at("2001-05-24 00:38:00 IST"), at("2001-05-24 00:38:00+02"), "GMT has no IST: the table's");
        stamp.apply().unwrap();
        assert_eq!(at("2001-05-24 00:38:00 IST"), at("2001-05-24 00:38:00+05:30"), "Asia/Kolkata's own IST");
    })
    .join()
    .unwrap();
}

/// V4R-452 (D1409a): under a claim, a collation the leader did not resolve (so not in the armed slice) is the named
/// ERROR, not a catcache lookup; none / the default / C / POSIX pass; off a claim (the leader, or after the guard drops)
/// every collation passes.
#[test]
fn an_unarmed_collation_on_a_claim_is_a_named_error() {
    use gravity_fn::typesem::collation::check_worker_collation;
    common::install_seams();
    mbutils::SetDatabaseEncoding(PG_UTF8 as _).unwrap();
    pg_locale::set_default_locale_builtin_utf8_for_tests();
    let stamp = StatementArm::capture();
    std::thread::spawn(move || {
        const UNRESOLVED: u32 = 12345;
        assert!(check_worker_collation(UNRESOLVED).is_ok(), "off a claim");
        let armed = stamp.arm().unwrap();
        let e = check_worker_collation(UNRESOLVED).expect_err("an unarmed collation under a claim");
        assert!(e.message().contains("D1409") && e.message().contains("12345"), "{}", e.message());
        for ok in [0, DEFAULT_COLLATION_OID, 950, 951] {
            assert!(check_worker_collation(ok).is_ok(), "collation {ok} is armed by default");
        }
        drop(armed);
        assert!(check_worker_collation(UNRESOLVED).is_ok(), "the claim ended");
    })
    .join()
    .unwrap();
}

/// D1367: a claim's guard puts back the default locale its arming moved. A retained pool thread that has run no
/// InitPostgres is bare again after the claim (so a later parallel query task's `init_database_collation` builds its own
/// database's default instead of keeping the leader's), and a thread with a default of its own gets that one back; a
/// claim that moved nothing leaves the thread's default as it is.
#[test]
fn a_claim_puts_back_the_default_locale_it_moved() {
    common::install_seams();
    mbutils::SetDatabaseEncoding(PG_UTF8 as _).unwrap();
    pg_locale::set_default_locale_builtin_utf8_for_tests();
    let stamp = StatementArm::capture();
    let is_stamp = || pg_locale::default_locale().is_some_and(|l| std::ptr::eq(l, &pg_locale::BUILTIN_C_UTF8_LOCALE));

    std::thread::spawn(move || {
        assert!(pg_locale::default_locale().is_none(), "a bare pool thread has no default locale");
        {
            let _armed = stamp.arm().unwrap();
            assert!(is_stamp(), "the claim runs under the leader's default");
        }
        assert!(pg_locale::default_locale().is_none(), "the bare thread is bare again after the claim");

        pg_locale::set_default_locale_c_for_tests(); // the thread's own database's default (a parallel query task's)
        let own = pg_locale::default_locale().expect("the thread's own default");
        {
            let _armed = stamp.arm().unwrap();
            assert!(is_stamp());
        }
        assert!(pg_locale::default_locale().is_some_and(|l| std::ptr::eq(l, own)), "the thread's own default is back");

        pg_locale::set_default_locale_builtin_utf8_for_tests();
        drop(stamp.arm().unwrap());
        assert!(is_stamp(), "a claim that moved nothing leaves the default");
    })
    .join()
    .unwrap();
}
