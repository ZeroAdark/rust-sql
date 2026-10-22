//! D2373: gravity_exec's session teardown (the backend levels and their `GravityAdmit` contexts) runs after every
//! Portals-phase cleanup, whichever registered first. The entry's `drain_parked` drops parked descs whose admissions
//! (key ExprStates) live in `GravityAdmit`; when a session's first Gravity contact was a nested ExecutorStart, the drain
//! registered before the first backend build and, Portals being LIFO, ran after the levels were gone (a write into freed
//! memory; the next session died).

use mcx::SessionCleanupPhase;
use pgsync::Mutex;
use std::thread::ThreadId;

/// Each test thread's registrations, in order: (thread, phase, label). The closures are dropped unrun: the test checks the
/// order the session teardown would run them in, not their bodies.
static REGS: Mutex<Vec<(ThreadId, SessionCleanupPhase, &'static str)>> = Mutex::new(Vec::new());
static NEXT_LABEL: Mutex<Option<&'static str>> = Mutex::new(None);

fn sink(p: SessionCleanupPhase, f: Box<dyn FnOnce()>) {
    drop(f);
    let label = pgsync::lock(&NEXT_LABEL).take().unwrap_or("unlabelled");
    pgsync::lock(&REGS).push((std::thread::current().id(), p, label)); // alloc: Cold
}

fn register(label: &'static str, phase: Option<SessionCleanupPhase>) {
    *pgsync::lock(&NEXT_LABEL) = Some(label);
    match phase {
        Some(p) => mcx::register_session_cleanup_phase(p, Box::new(|| {})), // alloc: Cold
        None => gravity_exec::ensure_session_end(),
    }
}

/// The run order of this thread's registrations as `mcxt_stats::run_session_teardown` drains them: Portals, then State,
/// then Roots; newest first within a phase.
fn teardown_order() -> Vec<&'static str> {
    let me = std::thread::current().id();
    let regs: Vec<_> = pgsync::lock(&REGS).iter().filter(|r| r.0 == me).map(|r| (r.1, r.2)).collect();
    let mut out = Vec::new();
    for phase in [SessionCleanupPhase::Portals, SessionCleanupPhase::State, SessionCleanupPhase::Roots] {
        out.extend(regs.iter().rev().filter(|r| r.0 == phase).map(|r| r.1)); // alloc: Cold
    }
    out
}

#[test]
fn d2373_session_end_runs_after_a_portals_drain_registered_before_the_first_backend_build() {
    mcx::set_session_cleanup_sink(sink);
    // the C32 prepared session: the first Gravity contact is a nested ExecutorStart, so the entry's drain of parked descs
    // registers before the first backend build registers gravity_exec's session end
    register("entry drain_parked", Some(SessionCleanupPhase::Portals));
    register("gravity_exec session_end", None);
    let order = teardown_order();
    let at = |l: &str| order.iter().position(|x| *x == l).expect(l);
    assert!(
        at("entry drain_parked") < at("gravity_exec session_end"),
        "the backend levels were torn down before a Portals cleanup that drops admissions allocated in them: {order:?}"
    );
}
