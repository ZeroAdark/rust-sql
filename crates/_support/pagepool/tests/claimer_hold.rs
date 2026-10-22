//! The claimer's prefault follows the ladder's raise rules: none while pressed or landing, then only the room past TOP.
#![cfg(target_os = "linux")]

mod common;
use common::*;
use pagepool::*;
use std::time::{Duration, Instant};

const TARGET: usize = 8 * MIB;

fn wait_for(what: &str, f: impl Fn(&Stats) -> bool) -> Stats {
    let t0 = Instant::now();
    loop {
        let s = stats();
        if f(&s) {
            return s;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "{what}: {s:?}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn the_rule_is_the_room_past_top_and_nothing_while_pressed_or_landing() {
    const G: u64 = 1 << 30;
    let (line, low, top) = (10 * G, G / 2, 2 * G);
    assert_eq!(claim_room(line, 7 * G, None, top, 0), G as usize, "room 3 GiB, TOP 2 GiB");
    assert_eq!(claim_room(line, 7 * G, Some(5 * G / 2), top, 0), (G / 2) as usize, "the window's lowest room bounds it");
    assert_eq!(claim_room(line, line - low / 2, None, top, 0), 0, "pressed (under LOW): nothing");
    assert_eq!(claim_room(line, line + G, None, top, 0), 0, "past the line (the cancel line, landing reads 0): nothing");
    assert_eq!(claim_room(line, 7 * G, None, top, 1), 0, "a release landing: nothing");
}

#[test]
fn the_claimer_holds_while_pressed_or_landing_and_resumes_past_top() {
    init_with(|c| {
        c.warm_zero = TARGET;
        c.warm_zero_max = TARGET;
        c.idle_drain_ms = 600_000;
    });
    set_claim_room(0);
    assert!(start());
    let s = wait_for("the claimer ran and was held", |s| s.claim_held >= 1);
    assert_eq!((s.warm_zero, s.prefaulted), (0, 0), "pressed: no prefault");
    let h = stats().claim_held;
    set_claim_room(2 * SUPER);
    let s = wait_for("the claimer filled the room, then held", |s| s.prefaulted >= 2 * SUPER as u64 && s.claim_held > h);
    assert_eq!((s.warm_zero, s.prefaulted), (2 * SUPER, 2 * SUPER as u64), "never past the room");
    hooks::set_landing(SUPER);
    let h = stats().claim_held;
    set_claim_room(usize::MAX);
    let s = wait_for("held while a release lands", |s| s.claim_held > h);
    assert_eq!(s.warm_zero, 2 * SUPER, "a release landing: no prefault, whatever the room");
    hooks::set_landing(0);
    set_claim_room(usize::MAX);
    let s = wait_for("resumed to the target", |s| s.warm_zero >= TARGET);
    assert_eq!(s.prefaulted, TARGET as u64);
    stop();
    balanced();
}
