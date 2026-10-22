//! Where a served statement's microseconds go: fixed marks on the backend thread (no allocation
//! on the marked path), one line per session (the first serve's) under PGRUST_GRAVITY_REPORT.
use core::cell::Cell;
use std::time::Instant;

/// Marks kept per statement for the phase report (a fixed thread-local array: no allocation on the marked path); later marks
/// are dropped from the report only. A report buffer bound.
const MAX: usize = gravity_program::cost::entry::PHASE_MARKS_MAX;

leader_tls::leader_tls! {
    thread_local! {
        static MARKS: [Cell<(&'static str, u64)>; MAX] = [const { Cell::new(("", 0)) }; MAX];
        static N: Cell<usize> = const { Cell::new(0) };
        static T0: Cell<Option<Instant>> = const { Cell::new(None) };
    }
}

/// Starts a statement's marks. Unless someone reads them (`PGRUST_GRAVITY_PHASES`'s per-statement line, or
/// `PGRUST_GRAVITY_REPORT`'s first-serve line) the marks stay off: no clock read per mark (H16 (a), 0.3 µs a served statement).
pub fn reset() {
    let t0 = armed().then(Instant::now);
    T0.with(|t| t.set(t0));
    N.with(|n| n.set(0));
}

/// Whether the marks are read at all (either switch). pglserve (D4601): also whether a parallel run reads the census
/// its served line and PHASES line print (`runner::drive::run_parallel`).
pub(crate) fn armed() -> bool {
    let (phases, report) = switches();
    phases || report
}

pub fn mark(label: &'static str) {
    let Some(t0) = T0.with(Cell::get) else { return };
    let us = t0.elapsed().as_micros() as u64;
    N.with(|n| {
        let i = n.get();
        if i < MAX {
            MARKS.with(|m| m[i].set((label, us)));
            n.set(i + 1);
        }
    });
}

/// `label=delta_us` pairs, deltas between consecutive marks (built once per session).
pub fn line() -> String {
    let n = N.with(Cell::get);
    let mut out = String::new(); // alloc: Cold
    let mut prev = 0u64;
    for i in 0..n {
        let (label, us) = MARKS.with(|m| m[i].get());
        out.push_str(&format!(" {label}={}", us - prev)); // alloc: Cold
        prev = us;
    }
    out
}

/// `PGRUST_GRAVITY_PHASES=1` (the section driver's switch, read once): the per-statement `PHASES stmt` line.
pub fn on() -> bool {
    switches().0
}

/// The switches that read the marks, the process environment's, read once: `PGRUST_GRAVITY_PHASES` (`on`) and
/// `PGRUST_GRAVITY_REPORT` (the first serve's line).
fn switches() -> (bool, bool) {
    static ON: pgsync::OnceLock<(bool, bool)> = pgsync::OnceLock::new();
    *ON.get_or_init(|| {
        let phases = gravity_driver::env::set("PGRUST_GRAVITY_PHASES");
        // the entry's build / apply clock reads only for this line (a statement's `reset` reads the switches first)
        gravity_driver::TIMED.store(phases, core::sync::atomic::Ordering::Relaxed);
        (phases, crate::inputs::knobs::seam::report_on())
    })
}
