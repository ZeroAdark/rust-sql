//! testflow (main 2026-10-11T09:33:11Z): a `poll-gap:` line names its gap's site even when no label held it (where the
//! stretch began and ended), sums every labelled step per label (a run of short steps shows), and reports the part no
//! label covers and the thread's on-CPU share. The child half runs the cases with the probe on (it logs to stderr); the
//! parent half reads its lines. One test: the switch is process-wide.
//! D4079: the file is named cancel_* so its binary runs in the crate tests' serial tail (CHECK_CT_SERIAL 'cancel'), alone
//! after e2e: its gaps and on-CPU shares are wall-clock readings, which nice-10 preemption beside e2e can skew.
use core::sync::atomic::AtomicU8;

const CHILD: &str = "CANCELPOLL_PROBE_LINE_CHILD";
const NAME: &str = "a_poll_gap_line_names_its_site_sums_its_steps_and_its_cpu";

fn busy(ms: u64) {
    let t0 = pg_clock::mono_ns();
    while pg_clock::mono_ns() - t0 < ms * 1_000_000 {
        core::hint::spin_loop();
    }
}

/// The `<n>` after `key` in `line` (`unlabelled 12 ms` -> 12).
fn num(line: &str, key: &str) -> u64 {
    let rest = &line[line.find(key).unwrap_or_else(|| panic!("no {key:?} in {line}")) + key.len()..];
    rest.trim_start().split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap()
}

fn child() {
    let byte = AtomicU8::new(0);
    cancelpoll::set_probe_ms(20);
    // 1: an unlabelled spin between two polls
    cancelpoll::scoped(&byte, || {
        cancelpoll::polled();
        busy(40);
        cancelpoll::polled();
    });
    // 2: ten 4 ms steps of one label, each under half the threshold, between two polls
    cancelpoll::scoped(&byte, || {
        cancelpoll::polled();
        for _ in 0..10 {
            let _s = cancelpoll::phase("short step");
            busy(4);
        }
        assert!(!cancelpoll::canceled());
    });
    // 3: a sleep (off-CPU) between the kernel call's start and its end
    cancelpoll::scoped(&byte, || std::thread::sleep(std::time::Duration::from_millis(60)));
    // 4: a spin into an interruptible wait (D3962), the wait, a spin after it: the wait's site ends the first gap and
    // begins the second, and the time inside the wait is in neither
    cancelpoll::scoped(&byte, || {
        cancelpoll::polled();
        busy(30);
        let wait = cancelpoll::waiting();
        std::thread::sleep(std::time::Duration::from_millis(40));
        drop(wait);
        busy(30);
        cancelpoll::polled();
    });
    cancelpoll::set_probe_ms(0);
}

#[test]
fn a_poll_gap_line_names_its_site_sums_its_steps_and_its_cpu() {
    if std::env::var_os(CHILD).is_some() {
        child();
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture", "--test-threads", "1"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "the child failed: {err}");
    let lines: Vec<&str> = err.lines().filter(|l| l.starts_with("poll-gap: ")).collect();
    assert_eq!(lines.len(), 5, "five gaps: {err}");

    let l = lines[0];
    assert!(l.contains(", ended by a poll;") && !l.contains("held by") && !l.contains("; inside"), "{l}");
    assert!(num(l, "; unlabelled ") >= 35, "the whole gap is unlabelled: {l}");
    assert!(l.contains("; began after a poll at ") && l.contains("cancel_probe_line.rs:"), "the stretch's start site: {l}");
    assert!(l.contains("; ended at ") && l.matches("cancel_probe_line.rs:").count() == 2, "and its end site: {l}");
    assert!(num(l, "; on-CPU ") >= 20, "a spin is on-CPU: {l}");

    let l = lines[1];
    assert!(l.contains("; held by short step ") && l.contains("(depth 0)"), "{l}");
    assert!(num(l, "; held by short step ") >= 35, "the ten steps are summed: {l}");
    assert!(l.contains("; steps short step x10 longest "), "{l}");
    assert!(num(l, " longest ") < 10, "each step is short: {l}");
    assert!(num(l, "; unlabelled ") <= 4, "the steps cover the gap: {l}");

    let l = lines[2];
    assert!(l.contains("ended by the kernel call's end") && l.contains("; began after the kernel call's start at "), "{l}");
    assert!(num(l, "; unlabelled ") >= 55, "{l}");
    let (cpu, wall) = (num(l, "; on-CPU "), num(l, " ms of "));
    assert!(wall >= 55 && cpu * 2 < wall, "a sleep is off-CPU: {l}");

    let l = lines[3];
    assert!(l.contains(", ended by an interruptible wait's start;") && l.contains("; began after a poll at "), "{l}");
    assert!(l.contains("; ended at ") && l.matches("cancel_probe_line.rs:").count() == 2, "the wait's site ends it: {l}");
    let l = lines[4];
    assert!(l.contains(", ended by a poll;") && l.contains("; began after an interruptible wait's end at "), "{l}");
    assert!(l.matches("cancel_probe_line.rs:").count() == 2, "the wait's site begins it: {l}");
    assert!(num(l, "poll-gap: ") < 60, "the 40 ms inside the wait is no part of the gap: {l}");
}
