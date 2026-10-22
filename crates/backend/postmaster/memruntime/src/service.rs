//! H3 step (1) (`PGRUST_MEMPOLICY=give_back_service=1`; default 0, its flip is the user's): the give-back service's
//! core (GIVEBACK-SERVICE-DESIGN, the broker's and memory accounting's conditions of 2026-10-17). It runs on memruntime's
//! page releaser thread (pg:memrelease, `release.rs`). The knob starts that thread as cache_release_bg does and moves the
//! cache images' own runs over to the service: knob 1 implies cache_release_bg's deferral, while cache_release_bg's own
//! knob and default are untouched. With this knob 0 nothing here runs. The core:
//! - One queue: `LANES`, one lane per job kind, fixed at build. Step (1) has one lane, the own runs: D4692's landing
//!   stacks (`allocarena::own`), static maps that never grow. The enqueue is the free's `leave` and the give is
//!   `drain_landing_upto`'s, both D4692's and unchanged, so the REAPING row's own-run posts are D4692's and nothing new
//!   is written to the ledger.
//! - One landing account: `landing()`, the lanes' bytes enqueued and not yet given back, the ladder's landing credit
//!   (`mcx::ladder::set_unmap_landing`, until the driver's start installs `reap::landing_credit`; that one counts the
//!   own runs too). Step (1): exactly `allocarena::own::landing_bytes()`. Past the watchdog's cancel line the ladder counts no
//!   landing at all (`mcx::ladder::waive_wait`), so a full queue never holds R6 off.
//! - Pacing: release.rs's batches (BATCH_BYTES, BATCH_PAUSE_US), across the lanes in order, one give-back call at a time.
//! - Admission at the enqueue (`allocarena::own::set_service`): the backlog cap (`BACKLOG_HOT_CAPS` hot pools) and C3's
//!   dead-man rule (runs wait and this thread made no pass for `DEAD_MAN_NS`). A run either rule turns away is given back
//!   on its freeing thread, as with the knob off. It is counted (`counts`, the accounts view's give_back_* rows) and
//!   logged here, one line at most every `LOG_EVERY_NS`. So a bar of no decommit on a query thread holds only while
//!   neither fires: report both counters per arm. Make_room's drain of the landing past the governor's bar on the drawing
//!   thread (reclaim.rs, D4692) is counted the same way.
//! - Normal priority: the thread keeps the spawning thread's nice and policy (tests/give_back_service.rs). At nice 10, a
//!   saturating load starved the purger past its dead-man stretch (purge.rs).
//!
//! H3 step (2) (same knob; GIVEBACK-SERVICE-DESIGN step (2), the broker's C1-C4 and memory accounting's M1-M4): the
//! arenas' purges become the service's jobs. At its start the service takes every pgrust arena's purge as owner 2
//! (`allocarena::take_service_purge`, a CAS per arena: pg:mempurge starts first, so with purge (a) on its arenas stay its
//! own, each take refused, counted and logged once, and the service is inert for them). The collects on every other
//! thread then skip the service's arenas (mimalloc's purge-owned flag), and the service purges them:
//! - each pass, every `PURGE_TICK_MS` while it owns an arena: their expired ranges (the purge a freeing thread's page free
//!   ran, A1(a)), each call marking the owner alive for mimalloc's dead-man rule (silent 1 s: an ordinary collect purges
//!   the expired ranges, counted per arena);
//! - a forced job: a release (`allocarena::release`, `give_back`: give_back_caches, the idle timer, passivate_trim, a
//!   statement's end, a session's exit / park / retire, the init-file load, the postmaster after the join, the watchdog's
//!   second purge) sets one flag and wakes the service, which force-purges each of its arenas: today's forced purge of
//!   the same ranges, on this thread. Past the watchdog's cancel line (`mcx::ladder::past_cancel_line`, live) the release
//!   runs it on its caller as today (C2);
//! - one arena per batch, `release::BATCH_PAUSE_US` between two that purged.
//! Purge jobs carry no REAPING post and no landing credit: their bytes stay on allocator_retained (9d's A3); nothing here
//! writes the ledger (M2). The ladder's pressure purge stays owner-agnostic (C1), its bytes over owned arenas counted
//! apart (purged_by_pressure, M3). Mimalloc's own OS arenas (PGRUST_ALLOC_ARENAS=0, or past pgrust's arenas) cannot be
//! owned and keep their purge on the calling thread.
//!
//! H3 step (4) (`give_back_spill`, default 1 under this knob; GIVEBACK-SERVICE-DESIGN step (4), the broker's S1-S4 and
//! memory accounting's review): the queue's second lane is gravity_driver's (`mem::giveback`, registered through
//! `allocarena::own::set_job_lane` at its first job): an allocator region its owner gives back whole (a spill region given
//! back before the statement's end, a statement's grown arena chunks past the parked ones) is one job, its charge on
//! REAPING from the enqueue, its pages and block given back here in the same paced batches, on this thread's own unbound
//! binding (never inside an enter). Its bytes are in `landing()`. Its admission is the driver's: this cap
//! (`own::service_cap`), this dead-man rule (`own::service_stalled`), and past the cancel line the give stays on its caller.

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Whether the knob is on (`give_back_service`, read at start after `PGRUST_MEMPOLICY` is applied).
pub fn enabled() -> bool {
    mempolicy::consts::get(mempolicy::consts::K::GiveBackService) != 0
}

/// One job kind's lane: its bytes enqueued and not yet given back, and its give-back of at most `max` bytes, which
/// returns the bytes (a job is never split, so a batch may pass `max` by less than one job).
pub(crate) struct Lane {
    pub(crate) landing: fn() -> usize,
    pub(crate) drain_upto: fn(usize) -> usize,
}

/// The queue's lanes, in drain order. Step (1): the own runs. H3 step (4) (`give_back_spill`): then the job lane a crate
/// above registers (`allocarena::own::set_job_lane`: gravity_driver's whole-region give-backs, `mem::giveback`), 0 bytes
/// and no drain until it does.
pub(crate) static LANES: [Lane; 2] = [
    Lane { landing: allocarena::own::landing_bytes, drain_upto: allocarena::own::drain_landing_upto },
    Lane { landing: allocarena::own::job_landing_bytes, drain_upto: allocarena::own::drain_jobs_upto },
];

/// The one landing account: every lane's bytes enqueued and not yet given back (`own::landing_bytes()` plus the job
/// lane's, `own::job_landing_bytes()`).
pub fn landing() -> u64 {
    landing_of(&LANES)
}

fn landing_of(lanes: &[Lane]) -> u64 {
    lanes.iter().map(|l| (l.landing)() as u64).sum()
}

/// One batch: the lanes drained in order until `max` bytes are given back. The bytes.
pub(crate) fn drain_upto(max: usize) -> usize {
    drain_upto_of(&LANES, max)
}

fn drain_upto_of(lanes: &[Lane], max: usize) -> usize {
    let mut n = 0;
    for l in lanes {
        if n >= max {
            break;
        }
        n += (l.drain_upto)(max - n);
    }
    n
}

/// The backlog cap in hot pools (`allocarena::own`'s hot cap, max(64 MiB, 2 % of B)): max(512 MiB, 16 % of B). CHOSEN,
/// not tuned: the D4757 pair's knob-on arms (the 8 GiB coremem stress) held at most 425 MiB on the landing, and the
/// paced drain (~6 GiB/s) gives a full backlog back in 0.1-0.2 s.
pub const BACKLOG_HOT_CAPS: usize = 8;
/// C3's dead-man stretch: runs wait and this thread made no pass for this long, so a free gives its run back itself.
const DEAD_MAN_NS: u64 = 1_000_000_000;
/// The service's last pass (`mcx::ladder_stats::clock_ns`; 0 = not started).
static ALIVE_NS: AtomicU64 = AtomicU64::new(0);

/// The service made a pass now (its start, each wake or tick, each batch).
pub(crate) fn alive() {
    ALIVE_NS.store(mcx::ladder_stats::clock_ns(), Relaxed);
}

/// The service is in place (`release::start`, before the free's wake is set): its first pass stamped (the clock's epoch
/// set, so the check below never initializes it inside a free) and the admission installed. H3 step (2): its wake for a
/// forced purge, then the arenas' purge taken (`TAKE`: arenas taken, takes refused).
pub(crate) fn begin(wake: fn()) {
    alive();
    allocarena::own::set_service(stalled, BACKLOG_HOT_CAPS);
    allocarena::set_service_wake(wake, note_handoff);
    let (taken, refused) = allocarena::take_service_purge();
    TAKE.store((u64::from(taken) << 32) | u64::from(refused), Relaxed);
}

/// H3 step (2): the arenas the service took at its start and the takes refused (high and low 32 bits).
static TAKE: AtomicU64 = AtomicU64::new(0);
/// H3 step (2): the pass's cadence while the service owns an arena (C4's purger tick). CHOSEN: C4's.
const PURGE_TICK_MS: u64 = 50;
/// H3 step (2): the service's arena purges that purged something, its forced passes, and its own decommits through the
/// arenas' commit callback (calls, bytes), published from its loop (outside the allocator).
static PURGES: AtomicU64 = AtomicU64::new(0);
static FORCED_PASSES: AtomicU64 = AtomicU64::new(0);
static PURGED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// H3 step (2), the broker's C2 measure: the oldest busy-guard hand-off past the cancel line not yet served (clock ns, 0 =
/// none), and the hand-offs served with the longest wait from the hand-off to the end of the forced pass that served it
/// (BAR: 50 ms; past it, the caller's bounded retry of the guard).
static HANDOFF_NS: AtomicU64 = AtomicU64::new(0);
static HANDOFFS: AtomicU64 = AtomicU64::new(0);
static HANDOFF_MAX_US: AtomicU64 = AtomicU64::new(0);

/// A busy-guard hand-off now (allocarena's past-the-line forced purge, on the caller): its time, the oldest kept.
fn note_handoff() {
    let _ = HANDOFF_NS.compare_exchange(0, mcx::ladder_stats::clock_ns(), Relaxed, Relaxed);
}

/// H3 step (2): the loop's tick: `PURGE_TICK_MS` while the service owns an arena, else `idle_ms`.
pub(crate) fn tick_ms(idle_ms: u64) -> u64 {
    if allocarena::purge_owner_counts().0 != 0 { PURGE_TICK_MS } else { idle_ms }
}

/// H3 step (2): one pass over the service's arenas: a forced purge when a release asked for one, else their expired
/// ranges; `nap` between two arenas that purged.
pub(crate) fn purge_pass(nap: &mut dyn FnMut(core::time::Duration)) {
    if allocarena::purge_owner_counts().0 == 0 {
        return;
    }
    // the hand-off's stamp before the flag: a hand-off between the two is timed to a later pass (longer, never shorter)
    let handoff = HANDOFF_NS.swap(0, Relaxed);
    let force = allocarena::take_force_due();
    for i in 0..allocarena::PURGE_SLOTS {
        if allocarena::service_purge(i, force) {
            PURGES.fetch_add(1, Relaxed);
            nap(core::time::Duration::from_micros(crate::release::BATCH_PAUSE_US));
        }
    }
    if force {
        FORCED_PASSES.fetch_add(1, Relaxed);
    }
    if handoff != 0 {
        if force {
            HANDOFFS.fetch_add(1, Relaxed);
            HANDOFF_MAX_US.fetch_max(mcx::ladder_stats::clock_ns().saturating_sub(handoff) / 1000, Relaxed);
        } else {
            // its flag not seen yet: the next pass serves it
            let _ = HANDOFF_NS.compare_exchange(0, handoff, Relaxed, Relaxed);
        }
    }
    let (n, b) = allocarena::thread_decommits();
    PURGED[0].store(n, Relaxed);
    PURGED[1].store(b, Relaxed);
}

/// H3 step (2): the takes refused at the start, logged once on the service's thread (its first pass).
pub(crate) fn log_owner_refusals() {
    let t = TAKE.load(Relaxed);
    let (taken, refused) = (t >> 32, t & 0xffff_ffff);
    if refused != 0 {
        write(&format!(
            "give-back service: {refused} arenas' purge is pg:mempurge's (purge (a) started first): the service leaves them on that path and owns {taken}"
        ));
    }
}

/// The admission's liveness check (`allocarena::own::set_service`): called inside a free while runs wait, so a clock read
/// and a load only.
fn stalled() -> bool {
    stalled_at(ALIVE_NS.load(Relaxed), mcx::ladder_stats::clock_ns())
}

fn stalled_at(alive_ns: u64, now_ns: u64) -> bool {
    alive_ns != 0 && now_ns.saturating_sub(alive_ns) > DEAD_MAN_NS
}

/// The service's counters (the accounts view's give_back_* rows, the log line).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// The landing account now, and the backlog cap now (bytes; 0 = no service).
    pub landing: u64,
    pub cap: u64,
    /// Batches that gave bytes back on the service's thread, and their bytes.
    pub batches: u64,
    pub given: u64,
    /// Runs turned away at the backlog cap, and their bytes (given back on their freeing threads).
    pub cap_runs: u64,
    pub cap_bytes: u64,
    /// Runs turned away with the service silent past its dead-man stretch, and their bytes (likewise).
    pub dead_runs: u64,
    pub dead_bytes: u64,
    /// Drains of the landing on a drawing thread (make_room past the governor's bar), and their bytes.
    pub drawer_drains: u64,
    pub drawer_bytes: u64,
    /// H3 step (2): the arenas the service owns, the owner takes refused (either owner's), the forced purges handed to the
    /// service and those run on their callers past the cancel line, the ladder's pressure purge's bytes from owned arenas.
    pub arenas_owned: u64,
    pub owner_refusals: u64,
    pub forced_jobs: u64,
    pub forced_inline: u64,
    /// Of those, the ones that met mimalloc's purge guard held by another thread (handed to the service).
    pub forced_guard_busy: u64,
    pub pressure_owned_bytes: u64,
    /// H3 step (2), C2's measure: the busy-guard hand-offs served, and the longest from a hand-off to the end of the forced
    /// pass that served it (us; BAR 50 ms).
    pub forced_handoffs: u64,
    pub forced_handoff_max_us: u64,
    /// H3 step (2): the service's arena purges that purged, its forced passes, its decommits (calls, bytes).
    pub purges: u64,
    pub forced_passes: u64,
    pub purged_calls: u64,
    pub purged_bytes: u64,
}

pub fn counts() -> Counts {
    let f = allocarena::own::service_fallbacks();
    let (drawer_drains, drawer_bytes) = allocarena::own::drawer_drains();
    let (batches, given) = crate::release::counts();
    let (arenas_owned, owner_refusals, forced_jobs, forced_inline, pressure_owned_bytes) = allocarena::purge_owner_counts();
    Counts {
        landing: landing(),
        cap: allocarena::own::service_cap() as u64,
        batches,
        given,
        cap_runs: f[0],
        cap_bytes: f[1],
        dead_runs: f[2],
        dead_bytes: f[3],
        drawer_drains,
        drawer_bytes,
        arenas_owned,
        owner_refusals,
        forced_jobs,
        forced_inline,
        forced_guard_busy: allocarena::forced_guard_busy(),
        forced_handoffs: HANDOFFS.load(Relaxed),
        forced_handoff_max_us: HANDOFF_MAX_US.load(Relaxed),
        pressure_owned_bytes,
        purges: PURGES.load(Relaxed),
        forced_passes: FORCED_PASSES.load(Relaxed),
        purged_calls: PURGED[0].load(Relaxed),
        purged_bytes: PURGED[1].load(Relaxed),
    }
}

/// The log's rate: at most one line this often, with the counts moved since the line before.
const LOG_EVERY_NS: u64 = 10_000_000_000;

/// The log's state on the service's thread: the counts at the last line, and when (0 = no line yet).
#[derive(Default)]
pub(crate) struct Log {
    last: Counts,
    at_ns: u64,
}

/// The service's tick: a line when a fallback or a drawer drain happened since the last line, at most every
/// `LOG_EVERY_NS`.
pub(crate) fn log(l: &mut Log) {
    if let Some(line) = next_line(l, &counts(), mcx::ladder_stats::clock_ns()) {
        write(&line);
    }
}

/// The next line at `now_ns` with the counts `c`, if one is due (and the state moved on to it).
fn next_line(l: &mut Log, c: &Counts, now_ns: u64) -> Option<String> {
    let moved = (c.cap_runs, c.dead_runs, c.drawer_drains, c.forced_inline) != (l.last.cap_runs, l.last.dead_runs, l.last.drawer_drains, l.last.forced_inline);
    if !moved || (l.at_ns != 0 && now_ns.saturating_sub(l.at_ns) < LOG_EVERY_NS) {
        return None;
    }
    let p = l.last;
    let mib = |b: u64| b >> 20;
    let line = format!(
        "give-back service: since the last line, {} runs ({} MiB) given back on their freeing threads at the backlog cap ({} MiB), \
         {} runs ({} MiB) with the service silent past its dead-man stretch (1 s), {} drains ({} MiB) of the landing on a \
         drawing thread (make_room past the governor's bar), {} forced arena purges run on their callers past the cancel \
         line; landing now {} MiB",
        c.cap_runs - p.cap_runs,
        mib(c.cap_bytes - p.cap_bytes),
        mib(c.cap),
        c.dead_runs - p.dead_runs,
        mib(c.dead_bytes - p.dead_bytes),
        c.drawer_drains - p.drawer_drains,
        mib(c.drawer_bytes - p.drawer_bytes),
        c.forced_inline - p.forced_inline,
        mib(c.landing)
    );
    l.last = *c;
    l.at_ns = now_ns;
    Some(line)
}

/// The service's thread stopped (a panic): from the dead-man stretch on, every free gives its run back itself.
pub(crate) fn log_stopped() {
    write("give-back service: its thread stopped (a panic); from 1 s on every free gives its run back on its own thread (the dead-man rule, counted)");
}

/// One LOG line through elog (the postmaster's prefix, set on this thread at its start); raw stderr if reporting unwinds.
fn write(line: &str) {
    // unwind-ok: log-then-die
    if std::panic::catch_unwind(|| {
        let _ = elog::elog(types_error::LOG, line.to_string());
    })
    .is_err()
    {
        elog::write_stderr(&format!("LOG:  {line}\n"));
    }
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;
    use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    use super::{Counts, Lane, Log};

    const MIB: u64 = 1 << 20;

    static A: AtomicUsize = AtomicUsize::new(0);
    static B: AtomicUsize = AtomicUsize::new(0);
    /// A lane of 512 KiB jobs whose backlog is a static.
    fn drain_of(s: &AtomicUsize, max: usize) -> usize {
        let mut n = 0;
        while n < max && s.load(Relaxed) >= 512 << 10 {
            s.fetch_sub(512 << 10, Relaxed);
            n += 512 << 10;
        }
        n
    }
    static FAKE: [Lane; 2] = [
        Lane { landing: || A.load(Relaxed), drain_upto: |max| drain_of(&A, max) },
        Lane { landing: || B.load(Relaxed), drain_upto: |max| drain_of(&B, max) },
    ];

    /// 9d's A1: step (1)'s lane is the own runs, so with no job lane registered (H3 step (4)'s, gravity_driver's) the one
    /// landing account is `allocarena::own::landing_bytes()`.
    #[test]
    fn step_1_the_landing_account_is_the_own_runs_landing() {
        assert_eq!(super::LANES.len(), 2, "the own runs, then step (4)'s job lane");
        assert_eq!(allocarena::own::job_landing_bytes(), 0, "no job lane registered here");
        assert_eq!(super::landing(), allocarena::own::landing_bytes() as u64);
        assert!(!super::enabled(), "give_back_service defaults to 0");
    }

    /// The queue's account sums its lanes, and a batch drains them in order up to its cap (a job never split).
    #[test]
    fn the_account_sums_the_lanes_and_a_batch_drains_them_in_order() {
        let _one = crate::tests::GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        A.store(3 << 20, Relaxed);
        B.store(2 << 20, Relaxed);
        assert_eq!(super::landing_of(&FAKE), 5 * MIB);
        assert_eq!(super::drain_upto_of(&FAKE, 4 << 20), 4 << 20, "one batch: the first lane's 3 MiB, then 1 MiB of the next");
        assert_eq!((A.load(Relaxed), B.load(Relaxed)), (0, 1 << 20));
        assert_eq!(super::drain_upto_of(&FAKE, 4 << 20), 1 << 20);
        assert_eq!(super::landing_of(&FAKE), 0);
    }

    /// C3's dead-man rule: stalled once the service made no pass for over 1 s, never before its first pass.
    #[test]
    fn the_service_is_stalled_only_past_its_dead_man_stretch() {
        let s = 1_000_000_000;
        assert!(!super::stalled_at(0, 5 * s), "not started: never stalled");
        assert!(!super::stalled_at(2 * s, 3 * s), "a pass 1 s ago: alive");
        assert!(super::stalled_at(2 * s, 3 * s + 1), "past the stretch: stalled");
        assert!(!super::stalled_at(4 * s, 3 * s), "a pass after the reading clock's: alive");
    }

    /// The broker's condition 2: never silent. A fallback or a drawer drain gives one line at once, then at most one
    /// every 10 s with what moved since; nothing moved, no line.
    #[test]
    fn a_fallback_is_logged_at_once_then_at_most_every_ten_seconds() {
        let s = 1_000_000_000u64;
        let mut l = Log::default();
        let mut c = Counts { landing: 8 * MIB, cap: 512 * MIB, ..Counts::default() };
        assert_eq!(super::next_line(&mut l, &c, s), None, "nothing turned away: no line");
        c.cap_runs = 3;
        c.cap_bytes = 3 * 512 << 10;
        let line = super::next_line(&mut l, &c, 2 * s).expect("the first fallback: a line at once");
        assert!(line.contains("3 runs (1 MiB) given back on their freeing threads at the backlog cap (512 MiB)"), "{line}");
        c.dead_runs = 1;
        c.dead_bytes = 512 << 10;
        assert_eq!(super::next_line(&mut l, &c, 5 * s), None, "within 10 s of the last line");
        let line = super::next_line(&mut l, &c, 12 * s).expect("10 s on: the next line");
        assert!(line.contains("0 runs (0 MiB) given back on their freeing threads") && line.contains("1 runs (0 MiB) with the service silent"), "{line}");
        c.drawer_drains = 2;
        c.drawer_bytes = 6 * MIB;
        let line = super::next_line(&mut l, &c, 30 * s).expect("a drawer drain: a line");
        assert!(line.contains("2 drains (6 MiB) of the landing on a drawing thread"), "{line}");
        assert_eq!(super::next_line(&mut l, &c, 60 * s), None, "nothing moved since");
    }

    /// The broker's condition 1: the service's landing never holds R6 off past the watchdog's cancel line. A queue with a
    /// backlog that covers the ladder's need: past the cancel line R6 runs in the same pass; under it the same backlog is
    /// landing and nobody is canceled.
    #[test]
    fn past_the_cancel_line_a_full_queue_never_holds_r6_off() {
        use mcx::ladder::Rungs;
        use mempolicy::ladder::Rung;
        let _one = crate::tests::GLOBALS.lock().unwrap_or_else(|e| e.into_inner());
        struct Fake {
            read: u64,
            now: Cell<u64>,
            cancels: Cell<u32>,
            in_flight: Cell<bool>,
        }
        impl Rungs for Fake {
            fn read(&self) -> Option<u64> {
                Some(self.read)
            }
            fn landing(&self) -> u64 {
                super::landing_of(&FAKE)
            }
            fn reclaimable(&self) -> u64 {
                0
            }
            fn reclaim(&self, _bytes: u64) -> u64 {
                0
            }
            fn asker(&self) -> (u64, u64, u64) {
                (0, 0, 0)
            }
            fn revocable(&self) -> u64 {
                0
            }
            fn own_spill(&self, _bytes: u64) {}
            fn revoke(&self, _bytes: u64) {}
            fn alloc_return(&self) {}
            fn now_ms(&self) -> u64 {
                self.now.get()
            }
            fn cancel_in_flight(&self) -> bool {
                self.in_flight.get()
            }
            fn cancel(&self, _need: u64) -> bool {
                self.cancels.set(self.cancels.get() + 1);
                self.in_flight.set(true);
                true
            }
        }
        let line = 1000 * MIB;
        // 100 MiB past the ladder's line; 160 MiB enqueued and not yet given back (a stalled service's backlog)
        A.store(96 << 20, Relaxed);
        B.store(64 << 20, Relaxed);
        let f = Fake { read: line + 100 * MIB, now: Cell::new(90_000), cancels: Cell::new(0), in_flight: Cell::new(false) };
        assert!(f.landing() >= 100 * MIB, "the queue's landing covers the need");
        mcx::ladder::publish(mcx::ladder::ON, line);
        mcx::ladder::waive_wait();
        let c = mcx::ladder::hook_pass_with(&f, line, 90_000);
        let r6 = 1u16 << Rung::Cancel as u8;
        assert_eq!((c.ran & r6, f.cancels.get()), (r6, 1), "past the cancel line: R6 in the same pass, the landing counts for nothing");
        assert!(!mcx::ladder::past_cancel(), "one pass only");
        // under the cancel line the same backlog is landing: it covers the need, nobody else is canceled
        f.in_flight.set(false);
        f.now.set(90_100);
        assert_eq!(mcx::ladder::hook_pass_with(&f, line, 90_100), mcx::ladder::Climbed::default(), "under the cancel line: landing");
        assert_eq!(f.cancels.get(), 1);
        mcx::ladder::publish(mcx::ladder::OFF, 0);
        mcx::ladder::clear_narrow();
        A.store(0, Relaxed);
        B.store(0, Relaxed);
    }
}
