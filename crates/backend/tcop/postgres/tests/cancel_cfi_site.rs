//! D4128 (testflow, main 2026-10-12T05:53:24Z): a CHECK_FOR_INTERRUPTS inside a kernel call is a poll for Gravity's poll-gap
//! probe (D3962), and the stretch after it must begin at the CHECK_FOR_INTERRUPTS call's own line. BORN RED: before
//! #[track_caller] on check_for_interrupts / check_for_abort_interrupts / mark_poll, every such stretch began
//! 'after a poll at crates/backend/tcop/postgres/src/lib.rs:<mark_poll's line>', so D4128's rows could not name the code.
//! K11 (testflow, main 2026-10-12T21:08:00Z; D4214 / D4229): the same through the seam (postgres_seams::check_for_interrupts,
//! the 244 backend call sites' form) and through Gravity's cancelpoll::interrupts::check / check_abort. BORN RED: through
//! the seam's fn pointer every stretch began 'after a poll at crates/backend/tcop/postgres/src/lib.rs:312'.
//! The child half runs with the probe on (it logs to stderr); the parent reads its lines. Named cancel_* for the crate
//! tests' serial tail (D4079: wall-clock readings).
//! D4281: a stretch the test does not make (cold code pages under I/O pressure, before a scope's first check or past its
//! last poll) is not a check's: each check's stretch is found by its own line, not by a count of five.
use core::sync::atomic::AtomicU8;

const CHILD: &str = "TCOP_CFI_SITE_CHILD";
const NAME: &str = "a_check_for_interrupts_names_its_own_line_as_the_stretch_start";
const COLD: &str = "a_cold_stretch_beside_a_check_is_not_taken_for_its_own";

fn busy(ms: u64) {
    let t0 = std::time::Instant::now();
    while t0.elapsed() < std::time::Duration::from_millis(ms) {
        core::hint::spin_loop();
    }
}

fn child(cold: u64) -> Vec<u32> {
    let byte = AtomicU8::new(0);
    cancelpoll::set_probe_ms(20);
    let mut lines = vec![0; 5];
    cancelpoll::scoped(&byte, || {
        busy(cold);
        lines[0] = line!() + 1;
        postgres::check_for_interrupts().unwrap();
        busy(40);
        cancelpoll::polled();
        busy(cold);
    });
    cancelpoll::scoped(&byte, || {
        lines[1] = line!() + 1;
        postgres::check_for_abort_interrupts().unwrap();
        busy(40);
        cancelpoll::polled();
    });
    // K11 (D4214 / D4229): through the seam (the 244 backend call sites' form) and through Gravity's facade, with the
    // seams installed as a backend installs them; each stretch must begin at the test's own line, not at tcop's lib.rs
    postgres::init_seams();
    cancelpoll::scoped(&byte, || {
        lines[2] = line!() + 1;
        postgres_seams::check_for_interrupts::call().unwrap();
        busy(40);
        cancelpoll::polled();
    });
    cancelpoll::scoped(&byte, || {
        lines[3] = line!() + 1;
        cancelpoll::interrupts::check().unwrap();
        busy(40);
        cancelpoll::polled();
    });
    cancelpoll::scoped(&byte, || {
        lines[4] = line!() + 1;
        cancelpoll::interrupts::check_abort().unwrap();
        busy(40);
        cancelpoll::polled();
    });
    cancelpoll::set_probe_ms(0);
    lines
}

/// The child (`cold` ms of spinning before the first check and past the last poll, else 0) run as test `name`; its gaps judged.
fn run(name: &str, cold: u64) {
    if std::env::var_os(CHILD).is_some() {
        let l = child(cold);
        eprintln!("cfi-lines: {}", l.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(" "));
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads", "1"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "the child failed: {err}");
    let gaps: Vec<&str> = err.lines().filter(|l| l.starts_with("poll-gap: ")).collect();
    assert!(gaps.len() >= 5, "a gap for each check: {err}");
    let cfi = err.lines().find_map(|l| l.strip_prefix("cfi-lines: ")).expect("the child's line numbers");
    let what = [
        "check_for_interrupts",
        "check_for_abort_interrupts",
        "the seam's check_for_interrupts (K11)",
        "cancelpoll::interrupts::check (K11)",
        "cancelpoll::interrupts::check_abort (K11)",
    ];
    for (n, what) in cfi.split(' ').zip(what) {
        let want = format!("; began after a poll at {}:{n};", file!());
        assert!(gaps.iter().any(|l| l.contains(&want)), "a {what} stretch begins at its own call ({want}), not at mark_poll: {err}");
    }
    for l in &gaps {
        assert!(!l.contains("tcop/postgres/src/lib.rs"), "{l}");
    }
}

#[test]
fn a_check_for_interrupts_names_its_own_line_as_the_stretch_start() {
    run(NAME, 0);
}

/// D4281 (born red on the five-gap count): the cold-page stretches the precheck slots logged, at a scope's start and
/// past its last poll, made a sixth gap.
#[test]
fn a_cold_stretch_beside_a_check_is_not_taken_for_its_own() {
    run(COLD, 40);
}
