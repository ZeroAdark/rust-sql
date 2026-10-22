//! Parser + matcher unit tests. The invalid/valid corpus is the upstream

use super::*;

fn ok(s: &str) -> Entry {
    parse_schedule(s).unwrap_or_else(|| panic!("expected valid schedule: {s:?}"))
}

fn bad(s: &str) {
    assert!(parse_schedule(s).is_none(), "expected invalid schedule: {s:?}");
}

fn bits(list: &[i32], low: i32) -> u64 {
    list.iter().fold(0, |acc, &v| acc | (1 << (v - low)))
}

#[test]
fn regress_invalid_schedules() {
    bad(&"a".repeat(1000));
    bad("* * * *");
    bad("5 secondc");
    bad("50 seconds c");
    bad("*/0 10 * * *");
    bad("-1 * * * *");
    bad("*/-1 10 * * *");
    bad("*/60 10 * * *");
    bad("* * * 13 *");
    bad("* * * 0 *");
    bad("*/5000000000 10 * * *");
    bad("-1 seconds");
    bad("0 seconds");
    bad("60 seconds");
    bad("10000000000 seconds");
    bad("@restar");
    bad("0 11 $foo * *");
    bad("");
    bad("   ");
    bad("# comment only");
}

#[test]
fn regress_valid_schedules() {
    let e = ok("0 10 * * *");
    assert_eq!(e.minute, 1);
    assert_eq!(e.hour, 1 << 10);
    assert_eq!(e.dom, bits(&(1..=31).collect::<Vec<_>>(), FIRST_DOM));
    assert_eq!(e.month, bits(&(1..=12).collect::<Vec<_>>(), FIRST_MONTH));
    assert_eq!(e.dow, bits(&(0..=7).collect::<Vec<_>>(), FIRST_DOW));
    assert_eq!(e.flags, DOM_STAR | DOW_STAR);
    assert!(!e.is_interval());

    let e = ok("* * * * *");
    assert_eq!(e.flags, MIN_STAR | HR_STAR | DOM_STAR | DOW_STAR);

    assert!(ok("@restart").is_at_restart());
    assert!(ok("@reboot").is_at_restart());

    assert_eq!(ok("1 second").seconds_interval, 1);
    assert_eq!(ok(" 30 sEcOnDs ").seconds_interval, 30);
    assert_eq!(ok("59 seconds").seconds_interval, 59);
    assert_eq!(ok("17  seconds ").seconds_interval, 17);

    let e = ok("0 11 $ * *");
    assert_eq!(e.flags & DOM_LAST, DOM_LAST);
    assert_eq!(e.dom, 0);
    assert_eq!(e.hour, 1 << 11);
}

#[test]
fn interval_sscanf_quirks() {
    assert_eq!(try_parse_interval("2 second"), Some(2));
    assert_eq!(try_parse_interval("2 second "), None);
    assert_eq!(try_parse_interval("2 secondz"), None);
    assert_eq!(try_parse_interval("2 seconds x"), None);
    assert_eq!(try_parse_interval("+5 seconds"), Some(5));
    assert_eq!(try_parse_interval("5\t\tseconds"), Some(5));
    assert_eq!(try_parse_interval("5seconds"), Some(5));
    assert_eq!(try_parse_interval("seconds"), None);
}

#[test]
fn ranges_steps_and_names() {
    let e = ok("*/15 1-3 1,15 jan,DEC mon-fri");
    assert_eq!(e.minute, bits(&[0, 15, 30, 45], FIRST_MINUTE));
    assert_eq!(e.hour, bits(&[1, 2, 3], FIRST_HOUR));
    assert_eq!(e.dom, bits(&[1, 15], FIRST_DOM));
    assert_eq!(e.month, bits(&[1, 12], FIRST_MONTH));
    assert_eq!(e.dow, bits(&[1, 2, 3, 4, 5], FIRST_DOW));
    assert_eq!(e.flags, MIN_STAR);

    assert_eq!(ok("0 0 * * 7").dow, bits(&[0, 7], FIRST_DOW));
    assert_eq!(ok("0 0 * * sun").dow, bits(&[0, 7], FIRST_DOW));

    bad("1/20 * * * *");
    bad("5-64/30 * * * *");
    bad("0 24 * * *");
    bad("0 0 32 * *");
    bad("0 0 0 * *");
    bad("0 0 * foo *");
}

#[test]
fn trailing_text_and_comments_follow_vixie() {
    assert!(parse_cron_entry("0 0 * * * echo hi").is_some());
    assert!(parse_cron_entry("\n# note\n0 0 * * *").is_some());
    assert!(parse_cron_entry("   0 0 * * *").is_some());
}

#[test]
fn specials() {
    let e = ok("@hourly");
    assert_eq!(e.minute, 1);
    assert_eq!(e.hour, bits(&(0..=23).collect::<Vec<_>>(), FIRST_HOUR));
    assert_eq!(e.flags, HR_STAR);
    let e = ok("@daily");
    assert_eq!(e.minute, 1);
    assert_eq!(e.hour, 1);
    assert_eq!(e.flags, 0);
    let e = ok("@weekly");
    assert_eq!(e.dow, bits(&[0, 7], FIRST_DOW));
    let e = ok("@monthly");
    assert_eq!(e.dom, 1);
    assert_eq!(ok("@yearly").month, 0);
    bad("@fortnightly");
}

const fn t(minute: i32, hour: i32, dom: i32, month: i32, dow: i32) -> LocalTime {
    LocalTime { minute, hour, day_of_month: dom, month, day_of_week: dow }
}

#[test]
fn matches_fixed_and_wild() {
    let fixed = ok("30 4 * * *");
    assert!(fixed.matches(t(30, 4, 10, 6, 3), false, false, false, true));
    assert!(!fixed.matches(t(30, 4, 10, 6, 3), false, false, true, false));
    assert!(!fixed.matches(t(31, 4, 10, 6, 3), false, false, true, true));

    let wild = ok("* * * * *");
    assert!(wild.matches(t(0, 0, 1, 1, 0), false, false, true, false));
    assert!(!wild.matches(t(0, 0, 1, 1, 0), false, false, false, true));
}

#[test]
fn matches_dom_dow_logic() {
    let e = ok("0 0 15 * mon");
    assert!(e.matches(t(0, 0, 15, 6, 3), false, false, true, true));
    assert!(e.matches(t(0, 0, 13, 6, 1), false, false, true, true));
    assert!(!e.matches(t(0, 0, 13, 6, 1), false, true, true, true));
    assert!(e.matches(t(0, 0, 15, 6, 1), false, true, true, true));
    let e = ok("0 0 15 * *");
    assert!(!e.matches(t(0, 0, 13, 6, 1), false, false, true, true));
    assert!(e.matches(t(0, 0, 15, 6, 1), false, false, true, true));
}

#[test]
fn matches_last_day_of_month() {
    let e = ok("0 11 $ * *");
    assert!(e.matches(t(0, 11, 30, 6, 2), true, false, true, true));
    assert!(!e.matches(t(0, 11, 29, 6, 1), false, false, true, true));
    let e = ok("0 11 $ * fri");
    assert!(e.matches(t(0, 11, 30, 6, 2), true, false, true, true));
    assert!(e.matches(t(0, 11, 12, 6, 5), false, false, true, true));
    assert!(!e.matches(t(0, 11, 12, 6, 4), false, false, true, true));
}
