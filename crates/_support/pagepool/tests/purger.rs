//! The purger: nothing is released without pressure (no warm cap); `Release { N }` frees >= 90 % of N within 100 ms on
//! the purger thread (default N = 256 MiB; PAGEPOOL_BIG_TESTS=1: 2 GiB); the door saw every claimed byte.
//! D4842: the clock is judged on the time the purger and this thread had a cpu; when the time they spent waiting for one
//! (their schedstat run delay, a loaded host's) explains the overrun, the clock is SKIPPED LOUDLY, never judged either way.

mod common;
use common::*;
use core::sync::atomic::{AtomicI64, Ordering::*};
use pagepool::*;
use std::time::{Duration, Instant};

static DOOR: AtomicI64 = AtomicI64::new(0);

fn door(d: i64) {
    DOOR.fetch_add(d, Relaxed);
}

/// D4842: the time waiting for a cpu so far (ns; schedstat's run_delay) of this process's threads named `comm`, summed,
/// or of this thread with None. None where it is unreadable (no /proc; a kernel without sched_info reads 0, so the clock
/// is judged there as before).
fn run_delay(comm: Option<&str>) -> Option<u64> {
    let field = |p: &std::path::Path| std::fs::read_to_string(p).ok()?.split_whitespace().nth(1)?.parse::<u64>().ok();
    let Some(c) = comm else { return field(std::path::Path::new("/proc/thread-self/schedstat")) };
    let mut sum = None;
    for t in std::fs::read_dir("/proc/self/task").ok()?.flatten() {
        if std::fs::read_to_string(t.path().join("comm")).is_ok_and(|n| n.trim_end() == c) {
            sum = Some(sum.unwrap_or(0) + field(&t.path().join("schedstat"))?);
        }
    }
    sum
}

#[test]
fn purger() {
    set_door(door);
    init_with(|c| c.va = if big() { 4 << 30 } else { 1 << 30 });
    assert!(start(), "the pool's threads started");
    let size = if big() { 2usize << 30 } else { 256 * MIB };
    let h = Hint::new(Class::Process, Tag::BUFFER);
    let spans: Vec<_> = (0..size / (16 * MIB)).map(|_| span(16 * MIB, h).expect("span")).collect();
    for s in &spans {
        touch(s.as_ptr() as usize, s.len());
    }
    let first = spans[0].as_ptr() as usize;
    for s in spans {
        // SAFETY: ours, not used again.
        unsafe { give_span(s) };
    }
    let r0 = stats().released_bytes;
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(stats().released_bytes, r0, "nothing released without pressure");
    let waits = || Some(run_delay(Some("pg:poolpurge"))? + run_delay(None)?);
    let w0 = waits();
    let t0 = Instant::now();
    set_pressure(Pressure::Release { bytes: size, class: None });
    let goal = (size / 10 * 9) as u64;
    // D4842: bounded at 30 s, not 5, so a starved purger reaches the goal and the clock below says why it is late
    while stats().released_bytes - r0 < goal && t0.elapsed() < Duration::from_secs(30) {
        std::hint::spin_loop();
    }
    let ms = t0.elapsed().as_secs_f64() * 1e3;
    // the purger's and this thread's waits for a cpu meanwhile (they may overlap: an upper bound on the time lost)
    let waited = w0.zip(waits()).map(|(a, b)| b.saturating_sub(a) as f64 / 1e6);
    let got = stats().released_bytes - r0;
    eprintln!("MEASURED purge clock: {} MiB released in {ms:.1} ms ({:.1} ms/GiB) on pg:poolpurge", got >> 20, ms / (got as f64 / (1u64 << 30) as f64));
    assert!(got >= goal, "released {got} of {size}");
    match waited {
        _ if ms <= 100.0 => {}
        Some(w) if ms - w <= 100.0 => {
            // straight to fd 2, not eprintln!: libtest captures eprintln! and shows it only for a failed test, and a
            // SKIP passes, so the line would never reach the log
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "SKIP (D4842, a starved purger): >= 90 % took {ms:.1} ms, {w:.1} ms of it waiting for a cpu (the purger's and this thread's run delay): the clock not judged here"
            );
        }
        _ => panic!(">= 90 % took {ms:.1} ms ({} ms of it waiting for a cpu)", waited.map_or("unknown".to_string(), |w| format!("{w:.1}"))),
    }
    while stats().released_bytes - r0 < size as u64 && t0.elapsed() < Duration::from_secs(30) {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(stats().released_bytes - r0, size as u64, "the whole ask was released");
    assert_eq!(resident(first, 16 * MIB), 0);
    let s = stats();
    assert!(s.purge_ns > 0 && s.releases > 0);
    assert_eq!(DOOR.load(Relaxed), s.claimed as i64, "the door saw every claim and release");
    stop();
    balanced();
}
