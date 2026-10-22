//! testflow (K6 / TD-179, main 2026-10-12T19:43:19Z): a `poll-gap:` line's OS part covers the gap, not the line's own write.
//! The probe reads the thread's CPU clock and getrusage BEFORE it writes the line. Read after it, a server log on a real
//! file showed the line's own dirtied page as 8 blocks out, and the booker (tools/pollgap-book.py) classed a plain sleep as an
//! OS stall. The child's stderr is a fresh file here (the parent opens it), so the line is the file's first write and
//! dirties its first page. The OS part must still read 0 blocks out. Off Linux the line has no OS part and the check is
//! skipped. The file is named cancel_* so its binary runs in the crate tests' serial tail (D4079: wall-clock readings).
use core::sync::atomic::AtomicU8;

const CHILD: &str = "CANCELPOLL_PROBE_ORDER_CHILD";
const NAME: &str = "a_poll_gap_lines_os_part_leaves_out_its_own_write";

fn child() {
    let byte = AtomicU8::new(0);
    cancelpoll::set_probe_ms(20);
    // one gap: a sleep between two polls (off-CPU, no I/O of its own); the line goes to the fresh stderr file
    cancelpoll::scoped(&byte, || {
        cancelpoll::polled();
        std::thread::sleep(std::time::Duration::from_millis(40));
        cancelpoll::polled();
    });
    cancelpoll::set_probe_ms(0);
}

#[test]
fn a_poll_gap_lines_os_part_leaves_out_its_own_write() {
    if std::env::var_os(CHILD).is_some() {
        child();
        return;
    }
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("cancel_probe_order-{}.err", std::process::id()));
    let err_file = std::fs::File::create(&path).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture", "--test-threads", "1"])
        .env(CHILD, "1")
        .stderr(err_file)
        .output()
        .unwrap();
    let err = std::fs::read_to_string(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(out.status.success(), "the child failed: {err}");
    let lines: Vec<&str> = err.lines().filter(|l| l.starts_with("poll-gap: ")).collect();
    assert_eq!(lines.len(), 1, "one gap: {err}");
    assert!(err.starts_with("poll-gap: "), "the line is the stderr file's first write: {err}");
    if cfg!(target_os = "linux") {
        let l = lines[0];
        let at = l.find("; blocks ").unwrap_or_else(|| panic!("no OS part: {l}")) + "; blocks ".len();
        let blocks: Vec<u64> = l[at..].split(|c: char| !c.is_ascii_digit()).filter(|x| !x.is_empty()).take(2).map(|x| x.parse().unwrap()).collect();
        assert_eq!(blocks, vec![0, 0], "a sleep's OS part has no block I/O: the line's own write is not in it: {l}");
    }
}
