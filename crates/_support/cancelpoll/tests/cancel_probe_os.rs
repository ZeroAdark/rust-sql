//! testflow (D4128 / D4129, main 2026-10-12T05:53:24Z): a `poll-gap:` line carries the thread's OS reading over its window
//! on Linux: user / system CPU, minor / major faults, block I/O in / out, voluntary / involuntary switches and, with
//! schedstats, the run-queue wait. This is what tells an off-CPU stretch that waited in code from one where the OS stalled the thread. Off Linux the
//! line has no OS part. The child half runs the cases with the probe on (it logs to stderr); the parent half reads its
//! lines. The file is named cancel_* so its binary runs in the crate tests' serial tail (D4079: wall-clock readings).
use core::sync::atomic::AtomicU8;

const CHILD: &str = "CANCELPOLL_PROBE_OS_CHILD";
const NAME: &str = "a_poll_gap_line_carries_the_threads_os_reading";

fn busy(ms: u64) {
    let t0 = pg_clock::mono_ns();
    while pg_clock::mono_ns() - t0 < ms * 1_000_000 {
        core::hint::spin_loop();
    }
}

/// The `<n>` after `key` in `line`.
fn num(line: &str, key: &str) -> u64 {
    let rest = &line[line.find(key).unwrap_or_else(|| panic!("no {key:?} in {line}")) + key.len()..];
    rest.trim_start().split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap()
}

fn child() {
    let byte = AtomicU8::new(0);
    cancelpoll::set_probe_ms(20);
    // 1: a spin between two polls (on-CPU, user time)
    cancelpoll::scoped(&byte, || {
        cancelpoll::polled();
        busy(60);
        cancelpoll::polled();
    });
    // 2: sleeps between two polls (off-CPU, blocked: voluntary switches)
    cancelpoll::scoped(&byte, || {
        cancelpoll::polled();
        for _ in 0..4 {
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        cancelpoll::polled();
    });
    // 3: a 1 MiB file write between two polls (block I/O out: the dirtied page-cache pages), then a sleep to pass the threshold
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("cancel_probe_os-{}", std::process::id()));
    cancelpoll::scoped(&byte, || {
        cancelpoll::polled();
        std::fs::write(&path, vec![7u8; 1 << 20]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(25));
        cancelpoll::polled();
    });
    let _ = std::fs::remove_file(&path);
    cancelpoll::set_probe_ms(0);
}

#[test]
fn a_poll_gap_line_carries_the_threads_os_reading() {
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
    assert_eq!(lines.len(), 3, "three gaps: {err}");
    if cfg!(target_os = "linux") {
        let (spin, sleep, write) = (lines[0], lines[1], lines[2]);
        for l in &lines {
            let os = &l[l.find("; user ").unwrap_or_else(|| panic!("no OS part: {l}"))..];
            let ok = os.contains(" ms, system ") && os.contains("; faults ") && os.contains(" minor, ")
                && os.contains(" major; blocks ") && os.contains(" in, ") && os.contains(" out; switches ")
                && os.contains(" voluntary, ")
                && (os.ends_with(" involuntary") || (os.contains(" involuntary; run-queue wait ") && os.ends_with(" ms")));
            assert!(ok, "the OS part: {l}");
            // the OS part comes after the on-CPU part (pollgap-book.py reads the parts in any order, but keep the line stable)
            assert!(l.find("; on-CPU ").unwrap() < l.find("; user ").unwrap(), "{l}");
        }
        assert!(num(spin, "; user ") >= 30, "a spin is user time: {spin}");
        assert!(num(sleep, " out; switches ") >= 3, "each sleep blocks once: {sleep}");
        assert!(num(write, " in, ") >= 1, "a 1 MiB write dirties pages (block I/O out): {write}");
    } else {
        assert!(lines.iter().all(|l| !l.contains("; faults ")), "no OS part off Linux: {lines:?}");
    }
}
